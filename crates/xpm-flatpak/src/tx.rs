use crate::backend::update_kind;
use libflatpak::{
    gio, glib, prelude::*, Installation, RefKind, Transaction, TransactionErrorDetails,
    TransactionOperationType,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xpm_core::package::UpdateKind;

#[derive(Debug, Clone)]
pub enum Request {
    Install(String),
    Remove(String),
    Update(String),
    UpdateAll,
    Bundle(String),
    FlatpakRef(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Update,
    Remove,
}

#[derive(Debug, Clone)]
pub struct Op {
    pub name: String,
    pub branch: String,
    pub kind: OpKind,
    pub tag: UpdateKind,
    pub remote: String,
    pub download: u64,
    pub installed: u64,
    pub system: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Operations added to the list; `first` is the global index of the first one.
    Plan { first: usize, ops: Vec<Op> },
    Started(usize),
    Progress { index: usize, percent: i32, status: String },
    Done(usize),
    Failed { index: usize, error: String },
    Notice(String),
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub installed: u32,
    pub updated: u32,
    pub removed: u32,
    pub failed: u32,
    pub declined: bool,
    pub error: Option<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.failed == 0 && self.error.is_none() && !self.declined
    }

    fn failure(error: impl Into<String>) -> Self {
        Self { failed: 1, error: Some(error.into()), ..Self::default() }
    }
}

type Confirm = Rc<dyn Fn(&[Op]) -> bool>;

struct Pending {
    installation: Installation,
    system: bool,
    adds: Vec<Add>,
}

enum Add {
    Install { remote: String, ref_: String },
    Update(String),
    Uninstall(String),
    Bundle(String),
    Ref(Vec<u8>),
}

fn installations() -> Vec<(Installation, bool)> {
    let mut out = Vec::new();
    if let Ok(i) = Installation::new_system(gio::Cancellable::NONE) {
        out.push((i, true));
    }
    if let Ok(i) = Installation::new_user(gio::Cancellable::NONE) {
        out.push((i, false));
    }
    out
}

fn remote_names(inst: &Installation) -> Vec<String> {
    let mut names: Vec<String> = inst
        .list_remotes(gio::Cancellable::NONE)
        .map(|r| {
            r.iter()
                .filter(|r| !r.is_disabled() && !r.is_noenumerate())
                .filter_map(|r| r.name().map(|n| n.to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|n| n != "flathub");
    names
}

fn find_remote_ref(all: &[(Installation, bool)], name: &str) -> Option<(usize, String, String)> {
    for (idx, (inst, _)) in all.iter().enumerate() {
        for remote in remote_names(inst) {
            for kind in [RefKind::App, RefKind::Runtime] {
                for branch in ["stable", "master"] {
                    if let Ok(r) = inst.fetch_remote_ref_sync(&remote, kind, name, None, Some(branch), gio::Cancellable::NONE)
                        && let Some(f) = r.format_ref()
                    {
                        return Some((idx, remote, f.to_string()));
                    }
                }
            }
        }
    }
    None
}

fn installed_refs_named(all: &[(Installation, bool)], name: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (idx, (inst, _)) in all.iter().enumerate() {
        if let Ok(refs) = inst.list_installed_refs(gio::Cancellable::NONE) {
            for r in refs {
                if r.name().as_deref() == Some(name)
                    && let Some(f) = r.format_ref()
                {
                    out.push((idx, f.to_string()));
                }
            }
        }
    }
    out
}

fn resolve(requests: &[Request]) -> Result<Vec<Pending>, String> {
    let all = installations();
    if all.is_empty() {
        return Err("No Flatpak installation is available".into());
    }
    let mut adds: Vec<Vec<Add>> = all.iter().map(|_| Vec::new()).collect();
    for req in requests {
        match req {
            Request::Install(name) => {
                let (idx, remote, ref_) = find_remote_ref(&all, name)
                    .ok_or_else(|| format!("{name} was not found in any enabled remote"))?;
                adds[idx].push(Add::Install { remote, ref_ });
            }
            Request::Remove(name) => {
                let found = installed_refs_named(&all, name);
                if found.is_empty() {
                    return Err(format!("{name} is not installed"));
                }
                for (idx, r) in found {
                    adds[idx].push(Add::Uninstall(r));
                }
            }
            Request::Update(name) => {
                let found = installed_refs_named(&all, name);
                if found.is_empty() {
                    return Err(format!("{name} is not installed"));
                }
                for (idx, r) in found {
                    adds[idx].push(Add::Update(r));
                }
            }
            Request::UpdateAll => {
                for (idx, (inst, _)) in all.iter().enumerate() {
                    for remote in remote_names(inst) {
                        let _ = inst.list_remote_refs_sync(&remote, gio::Cancellable::NONE);
                    }
                    if let Ok(refs) = inst.list_installed_refs_for_update(gio::Cancellable::NONE) {
                        for r in refs {
                            if let Some(f) = r.format_ref() {
                                adds[idx].push(Add::Update(f.to_string()));
                            }
                        }
                    }
                }
            }
            Request::Bundle(path) => adds[0].push(Add::Bundle(path.clone())),
            Request::FlatpakRef(path) => {
                let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
                adds[0].push(Add::Ref(data));
            }
        }
    }
    Ok(all
        .into_iter()
        .zip(adds)
        .filter(|(_, a)| !a.is_empty())
        .map(|((installation, system), adds)| Pending { installation, system, adds })
        .collect())
}

fn split_ref(r: &str) -> (RefKind, String, String) {
    let mut p = r.split('/');
    let kind = if p.next() == Some("app") { RefKind::App } else { RefKind::Runtime };
    let name = p.next().unwrap_or_default().to_string();
    let _arch = p.next();
    (kind, name, p.next().unwrap_or_default().to_string())
}

/// Runs the requests as libflatpak transactions (system, then user), reporting typed events.
/// `confirm` is asked once, with the first transaction's plan; later ones follow the same answer.
pub fn run(
    requests: &[Request],
    on_event: impl Fn(Event) + 'static,
    confirm: impl Fn(&[Op]) -> bool + 'static,
    is_cancelled: impl Fn() -> bool + Send + 'static,
) -> Outcome {
    let pending = match resolve(requests) {
        Ok(p) => p,
        Err(e) => return Outcome::failure(e),
    };
    let on_event = Rc::new(on_event);
    let confirm: Confirm = Rc::new(confirm);
    let answer: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    let outcome = Rc::new(RefCell::new(Outcome::default()));
    let base = Rc::new(Cell::new(0usize));

    let cancellable = gio::Cancellable::new();
    let finished = Arc::new(AtomicBool::new(false));
    {
        let (c, finished) = (cancellable.clone(), finished.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if is_cancelled() {
                    c.cancel();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    let mut failure: Option<String> = None;
    for p in pending {
        let tx = match Transaction::for_installation(&p.installation, Some(&cancellable)) {
            Ok(tx) => tx,
            Err(e) => {
                failure = Some(e.message().to_string());
                break;
            }
        };
        tx.set_no_interaction(true);
        tx.add_default_dependency_sources();
        for add in &p.adds {
            let r = match add {
                Add::Install { remote, ref_ } => tx.add_install(remote, ref_, &[]),
                Add::Update(r) => tx.add_update(r, &[], None),
                Add::Uninstall(r) => tx.add_uninstall(r),
                Add::Bundle(path) => tx.add_install_bundle(&gio::File::for_path(path), None),
                Add::Ref(data) => tx.add_install_flatpakref(&glib::Bytes::from(data)),
            };
            if let Err(e) = r {
                failure = Some(e.message().to_string());
                break;
            }
        }
        if failure.is_some() {
            break;
        }
        if tx.is_empty() {
            continue;
        }

        let refs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let system = p.system;

        tx.connect_ready({
            let (on_event, confirm, answer, refs, base) = (on_event.clone(), confirm.clone(), answer.clone(), refs.clone(), base.clone());
            move |tx| {
                let ops: Vec<Op> = tx
                    .operations()
                    .into_iter()
                    .filter(|o| !o.is_skipped())
                    .map(|o| {
                        let r = o.get_ref().map(|g| g.to_string()).unwrap_or_default();
                        let (kind, name, branch) = split_ref(&r);
                        refs.borrow_mut().push(r);
                        Op {
                            tag: update_kind(kind, &name),
                            name,
                            branch,
                            kind: match o.operation_type() {
                                TransactionOperationType::Update => OpKind::Update,
                                TransactionOperationType::Uninstall => OpKind::Remove,
                                _ => OpKind::Install,
                            },
                            remote: o.remote().map(|g| g.to_string()).unwrap_or_default(),
                            download: o.download_size(),
                            installed: o.installed_size(),
                            system,
                        }
                    })
                    .collect();
                on_event(Event::Plan { first: base.get(), ops: ops.clone() });
                match answer.get() {
                    Some(a) => a,
                    None => {
                        let a = confirm(&ops);
                        answer.set(Some(a));
                        a
                    }
                }
            }
        });

        tx.connect_new_operation({
            let (on_event, refs, base) = (on_event.clone(), refs.clone(), base.clone());
            move |_, op, progress| {
                let r = op.get_ref().map(|g| g.to_string()).unwrap_or_default();
                let Some(local) = refs.borrow().iter().position(|x| *x == r) else { return };
                let index = base.get() + local;
                on_event(Event::Started(index));
                let on_event = on_event.clone();
                progress.connect_changed(move |p| {
                    on_event(Event::Progress {
                        index,
                        percent: p.progress().clamp(0, 100),
                        status: p.status().map(|s| s.to_string()).unwrap_or_default(),
                    });
                });
            }
        });

        tx.connect_operation_done({
            let (on_event, refs, base, outcome) = (on_event.clone(), refs.clone(), base.clone(), outcome.clone());
            move |_, op, _commit, _result| {
                let r = op.get_ref().map(|g| g.to_string()).unwrap_or_default();
                let Some(local) = refs.borrow().iter().position(|x| *x == r) else { return };
                let mut o = outcome.borrow_mut();
                match op.operation_type() {
                    TransactionOperationType::Update => o.updated += 1,
                    TransactionOperationType::Uninstall => o.removed += 1,
                    _ => o.installed += 1,
                }
                on_event(Event::Done(base.get() + local));
            }
        });

        tx.connect_operation_error({
            let (on_event, refs, base, outcome) = (on_event.clone(), refs.clone(), base.clone(), outcome.clone());
            move |_, op, err, details| {
                let r = op.get_ref().map(|g| g.to_string()).unwrap_or_default();
                if let Some(local) = refs.borrow().iter().position(|x| *x == r) {
                    outcome.borrow_mut().failed += 1;
                    on_event(Event::Failed { index: base.get() + local, error: err.message().to_string() });
                }
                details.contains(TransactionErrorDetails::FATAL)
            }
        });

        tx.connect_add_new_remote(|_, _, _, _, _| true);
        tx.connect_end_of_lifed({
            let on_event = on_event.clone();
            move |_, r, reason, _| {
                let (_, name, _) = split_ref(r);
                on_event(Event::Notice(format!("{name} is end-of-life{}", if reason.is_empty() { String::new() } else { format!(": {reason}") })));
            }
        });

        let run = tx.run(Some(&cancellable));
        base.set(base.get() + refs.borrow().len());
        if answer.get() == Some(false) {
            outcome.borrow_mut().declined = true;
            break;
        }
        if let Err(e) = run {
            failure = Some(e.message().to_string());
            break;
        }
    }
    finished.store(true, Ordering::Relaxed);
    let mut outcome = outcome.borrow().clone();
    if let Some(e) = failure {
        outcome.error = Some(e);
    }
    outcome
}
