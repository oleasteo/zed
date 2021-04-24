mod char_bag;
mod fuzzy;

use crate::{
    editor::{History, Snapshot as BufferSnapshot},
    sum_tree::{self, Cursor, Edit, SeekBias, SumTree},
};
use anyhow::{anyhow, Context, Result};
pub use fuzzy::{match_paths, PathMatch};
use gpui::{scoped_pool, AppContext, Entity, ModelContext, ModelHandle, Task};
use ignore::gitignore::Gitignore;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use postage::{
    prelude::{Sink, Stream},
    watch,
};
use smol::{channel::Sender, Timer};
use std::{
    cmp,
    collections::{BTreeMap, HashSet},
    ffi::{CStr, OsStr},
    fmt, fs,
    future::Future,
    io::{self, Read, Write},
    mem,
    ops::{AddAssign, Deref},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use self::char_bag::CharBag;

lazy_static! {
    static ref GITIGNORE: &'static OsStr = OsStr::new(".gitignore");
}

#[derive(Clone, Debug)]
enum ScanState {
    Idle,
    Scanning,
    Err(Arc<io::Error>),
}

pub struct Worktree {
    snapshot: Snapshot,
    background_snapshot: Arc<Mutex<Snapshot>>,
    scan_state: (watch::Sender<ScanState>, watch::Receiver<ScanState>),
    _event_stream_handle: fsevent::Handle,
    poll_scheduled: bool,
}

#[derive(Clone)]
pub struct FileHandle {
    worktree: ModelHandle<Worktree>,
    path: Arc<Path>,
}

impl Worktree {
    pub fn new(path: impl Into<Arc<Path>>, ctx: &mut ModelContext<Self>) -> Self {
        let abs_path = path.into();
        let root_name_chars = abs_path.file_name().map_or(Vec::new(), |n| {
            n.to_string_lossy().chars().chain(Some('/')).collect()
        });
        let (scan_state_tx, scan_state_rx) = smol::channel::unbounded();
        let id = ctx.model_id();
        let snapshot = Snapshot {
            id,
            scan_id: 0,
            abs_path,
            root_name_chars,
            ignores: Default::default(),
            entries: Default::default(),
        };
        let (event_stream, event_stream_handle) =
            fsevent::EventStream::new(&[snapshot.abs_path.as_ref()], Duration::from_millis(100));

        let background_snapshot = Arc::new(Mutex::new(snapshot.clone()));

        let tree = Self {
            snapshot,
            background_snapshot: background_snapshot.clone(),
            scan_state: watch::channel_with(ScanState::Scanning),
            _event_stream_handle: event_stream_handle,
            poll_scheduled: false,
        };

        std::thread::spawn(move || {
            let scanner = BackgroundScanner::new(background_snapshot, scan_state_tx, id);
            scanner.run(event_stream)
        });

        ctx.spawn_stream(scan_state_rx, Self::observe_scan_state, |_, _| {})
            .detach();

        tree
    }

    pub fn scan_complete(&self) -> impl Future<Output = ()> {
        let mut scan_state_rx = self.scan_state.1.clone();
        async move {
            let mut scan_state = Some(scan_state_rx.borrow().clone());
            while let Some(ScanState::Scanning) = scan_state {
                scan_state = scan_state_rx.recv().await;
            }
        }
    }

    pub fn next_scan_complete(&self) -> impl Future<Output = ()> {
        let mut scan_state_rx = self.scan_state.1.clone();
        let mut did_scan = matches!(*scan_state_rx.borrow(), ScanState::Scanning);
        async move {
            loop {
                if let ScanState::Scanning = *scan_state_rx.borrow() {
                    did_scan = true;
                } else if did_scan {
                    break;
                }
                scan_state_rx.recv().await;
            }
        }
    }

    fn observe_scan_state(&mut self, scan_state: ScanState, ctx: &mut ModelContext<Self>) {
        let _ = self.scan_state.0.blocking_send(scan_state);
        self.poll_entries(ctx);
    }

    fn poll_entries(&mut self, ctx: &mut ModelContext<Self>) {
        self.snapshot = self.background_snapshot.lock().clone();
        ctx.notify();

        if self.is_scanning() && !self.poll_scheduled {
            ctx.spawn(Timer::after(Duration::from_millis(100)), |this, _, ctx| {
                this.poll_scheduled = false;
                this.poll_entries(ctx);
            })
            .detach();
            self.poll_scheduled = true;
        }
    }

    fn is_scanning(&self) -> bool {
        if let ScanState::Scanning = *self.scan_state.1.borrow() {
            true
        } else {
            false
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub fn contains_abs_path(&self, path: &Path) -> bool {
        path.starts_with(&self.snapshot.abs_path)
    }

    pub fn load_history(
        &self,
        path: &Path,
        ctx: &AppContext,
    ) -> impl Future<Output = Result<History>> {
        let abs_path = self.snapshot.abs_path.join(path);
        ctx.background_executor().spawn(async move {
            let mut file = std::fs::File::open(&abs_path)?;
            let mut base_text = String::new();
            file.read_to_string(&mut base_text)?;
            Ok(History::new(Arc::from(base_text)))
        })
    }

    pub fn save<'a>(
        &self,
        path: &Path,
        content: BufferSnapshot,
        ctx: &AppContext,
    ) -> Task<Result<()>> {
        let abs_path = self.snapshot.abs_path.join(path);
        ctx.background_executor().spawn(async move {
            let buffer_size = content.text_summary().bytes.min(10 * 1024);
            let file = std::fs::File::create(&abs_path)?;
            let mut writer = std::io::BufWriter::with_capacity(buffer_size, file);
            for chunk in content.fragments() {
                writer.write(chunk.as_bytes())?;
            }
            writer.flush()?;
            Ok(())
        })
    }
}

impl Entity for Worktree {
    type Event = ();
}

impl Deref for Worktree {
    type Target = Snapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.snapshot.fmt(f)
    }
}

#[derive(Clone)]
pub struct Snapshot {
    id: usize,
    scan_id: usize,
    abs_path: Arc<Path>,
    root_name_chars: Vec<char>,
    ignores: BTreeMap<Arc<Path>, (Arc<Gitignore>, usize)>,
    entries: SumTree<Entry>,
}

impl Snapshot {
    pub fn file_count(&self) -> usize {
        self.entries.summary().file_count
    }

    pub fn visible_file_count(&self) -> usize {
        self.entries.summary().visible_file_count
    }

    pub fn files(&self, start: usize) -> FileIter {
        FileIter::all(self, start)
    }

    #[cfg(test)]
    pub fn paths(&self) -> impl Iterator<Item = &Arc<Path>> {
        let mut cursor = self.entries.cursor::<(), ()>();
        cursor.next();
        cursor.map(|entry| entry.path())
    }

    pub fn visible_files(&self, start: usize) -> FileIter {
        FileIter::visible(self, start)
    }

    pub fn root_entry(&self) -> &Entry {
        self.entry_for_path("").unwrap()
    }

    pub fn root_name(&self) -> Option<&OsStr> {
        self.abs_path.file_name()
    }

    pub fn root_name_chars(&self) -> &[char] {
        &self.root_name_chars
    }

    fn entry_for_path(&self, path: impl AsRef<Path>) -> Option<&Entry> {
        let mut cursor = self.entries.cursor::<_, ()>();
        if cursor.seek(&PathSearch::Exact(path.as_ref()), SeekBias::Left) {
            cursor.item()
        } else {
            None
        }
    }

    pub fn inode_for_path(&self, path: impl AsRef<Path>) -> Option<u64> {
        self.entry_for_path(path.as_ref()).map(|e| e.inode())
    }

    fn is_path_ignored(&self, path: &Path) -> Result<bool> {
        let mut entry = self
            .entry_for_path(path)
            .ok_or_else(|| anyhow!("entry does not exist in worktree"))?;

        if path.starts_with(".git") {
            Ok(true)
        } else {
            while let Some(parent_entry) =
                entry.path().parent().and_then(|p| self.entry_for_path(p))
            {
                let parent_path = parent_entry.path();
                if let Some((ignore, _)) = self.ignores.get(parent_path) {
                    let relative_path = path.strip_prefix(parent_path).unwrap();
                    match ignore.matched_path_or_any_parents(relative_path, entry.is_dir()) {
                        ignore::Match::Whitelist(_) => return Ok(false),
                        ignore::Match::Ignore(_) => return Ok(true),
                        ignore::Match::None => {}
                    }
                }
                entry = parent_entry;
            }
            Ok(false)
        }
    }

    fn insert_entry(&mut self, entry: Entry) {
        if !entry.is_dir() && entry.path().file_name() == Some(&GITIGNORE) {
            self.insert_ignore_file(entry.path());
        }
        self.entries.insert(entry);
    }

    fn populate_dir(&mut self, parent_path: Arc<Path>, entries: impl IntoIterator<Item = Entry>) {
        let mut edits = Vec::new();

        let mut parent_entry = self.entries.get(&PathKey(parent_path)).unwrap().clone();
        if matches!(parent_entry.kind, EntryKind::PendingDir) {
            parent_entry.kind = EntryKind::Dir;
        } else {
            unreachable!();
        }
        edits.push(Edit::Insert(parent_entry));

        for entry in entries {
            if !entry.is_dir() && entry.path().file_name() == Some(&GITIGNORE) {
                self.insert_ignore_file(entry.path());
            }
            edits.push(Edit::Insert(entry));
        }
        self.entries.edit(edits);
    }

    fn remove_path(&mut self, path: &Path) {
        let new_entries = {
            let mut cursor = self.entries.cursor::<_, ()>();
            let mut new_entries = cursor.slice(&PathSearch::Exact(path), SeekBias::Left);
            cursor.seek_forward(&PathSearch::Successor(path), SeekBias::Left);
            new_entries.push_tree(cursor.suffix());
            new_entries
        };
        self.entries = new_entries;

        if path.file_name() == Some(&GITIGNORE) {
            if let Some((_, scan_id)) = self.ignores.get_mut(path.parent().unwrap()) {
                *scan_id = self.scan_id;
            }
        }
    }

    fn insert_ignore_file(&mut self, path: &Path) {
        let (ignore, err) = Gitignore::new(self.abs_path.join(path));
        if let Some(err) = err {
            log::error!("error in ignore file {:?} - {:?}", path, err);
        }

        let ignore_parent_path = path.parent().unwrap().into();
        self.ignores
            .insert(ignore_parent_path, (Arc::new(ignore), self.scan_id));
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for entry in self.entries.cursor::<(), ()>() {
            for _ in entry.path().ancestors().skip(1) {
                write!(f, " ")?;
            }
            writeln!(f, "{:?} (inode: {})", entry.path(), entry.inode())?;
        }
        Ok(())
    }
}

impl FileHandle {
    pub fn path(&self) -> &Arc<Path> {
        &self.path
    }

    pub fn load_history(&self, ctx: &AppContext) -> impl Future<Output = Result<History>> {
        self.worktree.read(ctx).load_history(&self.path, ctx)
    }

    pub fn save<'a>(&self, content: BufferSnapshot, ctx: &AppContext) -> Task<Result<()>> {
        let worktree = self.worktree.read(ctx);
        worktree.save(&self.path, content, ctx)
    }

    pub fn entry_id(&self) -> (usize, Arc<Path>) {
        (self.worktree.id(), self.path.clone())
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    kind: EntryKind,
    path: Arc<Path>,
    inode: u64,
    is_symlink: bool,
    is_ignored: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum EntryKind {
    PendingDir,
    Dir,
    File(CharBag),
}

impl Entry {
    pub fn path(&self) -> &Arc<Path> {
        &self.path
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    fn is_ignored(&self) -> Option<bool> {
        self.is_ignored
    }

    fn set_ignored(&mut self, ignored: bool) {
        self.is_ignored = Some(ignored);
    }

    fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::PendingDir)
    }
}

impl sum_tree::Item for Entry {
    type Summary = EntrySummary;

    fn summary(&self) -> Self::Summary {
        let file_count;
        let visible_file_count;
        if matches!(self.kind, EntryKind::File(_)) {
            file_count = 1;
            if self.is_ignored.unwrap_or(false) {
                visible_file_count = 0;
            } else {
                visible_file_count = 1;
            }
        } else {
            file_count = 0;
            visible_file_count = 0;
        }

        EntrySummary {
            max_path: self.path().clone(),
            file_count,
            visible_file_count,
            recompute_ignore_status: self.is_ignored().is_none(),
        }
    }
}

impl sum_tree::KeyedItem for Entry {
    type Key = PathKey;

    fn key(&self) -> Self::Key {
        PathKey(self.path().clone())
    }
}

#[derive(Clone, Debug)]
pub struct EntrySummary {
    max_path: Arc<Path>,
    file_count: usize,
    visible_file_count: usize,
    recompute_ignore_status: bool,
}

impl Default for EntrySummary {
    fn default() -> Self {
        Self {
            max_path: Arc::from(Path::new("")),
            file_count: 0,
            visible_file_count: 0,
            recompute_ignore_status: false,
        }
    }
}

impl<'a> AddAssign<&'a EntrySummary> for EntrySummary {
    fn add_assign(&mut self, rhs: &'a EntrySummary) {
        self.max_path = rhs.max_path.clone();
        self.file_count += rhs.file_count;
        self.visible_file_count += rhs.visible_file_count;
        self.recompute_ignore_status |= rhs.recompute_ignore_status;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PathKey(Arc<Path>);

impl Default for PathKey {
    fn default() -> Self {
        Self(Path::new("").into())
    }
}

impl<'a> sum_tree::Dimension<'a, EntrySummary> for PathKey {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        self.0 = summary.max_path.clone();
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PathSearch<'a> {
    Exact(&'a Path),
    Successor(&'a Path),
}

impl<'a> Ord for PathSearch<'a> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        match (self, other) {
            (Self::Exact(a), Self::Exact(b)) => a.cmp(b),
            (Self::Successor(a), Self::Exact(b)) => {
                if b.starts_with(a) {
                    cmp::Ordering::Greater
                } else {
                    a.cmp(b)
                }
            }
            _ => todo!("not sure we need the other two cases"),
        }
    }
}

impl<'a> PartialOrd for PathSearch<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Default for PathSearch<'a> {
    fn default() -> Self {
        Self::Exact(Path::new("").into())
    }
}

impl<'a: 'b, 'b> sum_tree::Dimension<'a, EntrySummary> for PathSearch<'b> {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        *self = Self::Exact(summary.max_path.as_ref());
    }
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FileCount(usize);

impl<'a> sum_tree::Dimension<'a, EntrySummary> for FileCount {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        self.0 += summary.file_count;
    }
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct VisibleFileCount(usize);

impl<'a> sum_tree::Dimension<'a, EntrySummary> for VisibleFileCount {
    fn add_summary(&mut self, summary: &'a EntrySummary) {
        self.0 += summary.visible_file_count;
    }
}

struct BackgroundScanner {
    snapshot: Arc<Mutex<Snapshot>>,
    notify: Sender<ScanState>,
    other_mount_paths: HashSet<PathBuf>,
    thread_pool: scoped_pool::Pool,
    root_char_bag: CharBag,
}

impl BackgroundScanner {
    fn new(snapshot: Arc<Mutex<Snapshot>>, notify: Sender<ScanState>, worktree_id: usize) -> Self {
        let root_char_bag = CharBag::from(snapshot.lock().root_name_chars.as_slice());
        let mut scanner = Self {
            root_char_bag,
            snapshot,
            notify,
            other_mount_paths: Default::default(),
            thread_pool: scoped_pool::Pool::new(16, format!("worktree-{}-scanner", worktree_id)),
        };
        scanner.update_other_mount_paths();
        scanner
    }

    fn update_other_mount_paths(&mut self) {
        let path = self.snapshot.lock().abs_path.clone();
        self.other_mount_paths.clear();
        self.other_mount_paths.extend(
            mounted_volume_paths()
                .into_iter()
                .filter(|mount_path| !path.starts_with(mount_path)),
        );
    }

    fn abs_path(&self) -> Arc<Path> {
        self.snapshot.lock().abs_path.clone()
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().clone()
    }

    fn run(mut self, event_stream: fsevent::EventStream) {
        if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
            return;
        }

        if let Err(err) = self.scan_dirs() {
            if smol::block_on(self.notify.send(ScanState::Err(Arc::new(err)))).is_err() {
                return;
            }
        }

        if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
            return;
        }

        event_stream.run(move |events| {
            if smol::block_on(self.notify.send(ScanState::Scanning)).is_err() {
                return false;
            }

            if !self.process_events(events) {
                return false;
            }

            if smol::block_on(self.notify.send(ScanState::Idle)).is_err() {
                return false;
            }

            true
        });
    }

    fn scan_dirs(&self) -> io::Result<()> {
        self.snapshot.lock().scan_id += 1;

        let path: Arc<Path> = Arc::from(Path::new(""));
        let abs_path = self.abs_path();
        let metadata = fs::metadata(&abs_path)?;
        let inode = metadata.ino();
        let is_symlink = fs::symlink_metadata(&abs_path)?.file_type().is_symlink();

        if metadata.file_type().is_dir() {
            let dir_entry = Entry {
                kind: EntryKind::PendingDir,
                path: path.clone(),
                inode,
                is_symlink,
                is_ignored: None,
            };
            self.snapshot.lock().insert_entry(dir_entry);

            let (tx, rx) = crossbeam_channel::unbounded();

            tx.send(ScanJob {
                abs_path: abs_path.to_path_buf(),
                path,
                scan_queue: tx.clone(),
            })
            .unwrap();
            drop(tx);

            self.thread_pool.scoped(|pool| {
                for _ in 0..self.thread_pool.thread_count() {
                    pool.execute(|| {
                        while let Ok(job) = rx.recv() {
                            if let Err(err) = self.scan_dir(&job) {
                                log::error!("error scanning {:?}: {}", job.abs_path, err);
                            }
                        }
                    });
                }
            });
        } else {
            self.snapshot.lock().insert_entry(Entry {
                kind: EntryKind::File(self.char_bag(&path)),
                path,
                inode,
                is_symlink,
                is_ignored: None,
            });
        }

        self.recompute_ignore_statuses();

        Ok(())
    }

    fn scan_dir(&self, job: &ScanJob) -> io::Result<()> {
        let mut new_entries = Vec::new();
        let mut new_jobs = Vec::new();

        for child_entry in fs::read_dir(&job.abs_path)? {
            let child_entry = child_entry?;
            let child_name = child_entry.file_name();
            let child_abs_path = job.abs_path.join(&child_name);
            let child_path: Arc<Path> = job.path.join(&child_name).into();
            let child_metadata = child_entry.metadata()?;
            let child_inode = child_metadata.ino();
            let child_is_symlink = child_metadata.file_type().is_symlink();

            // Disallow mount points outside the file system containing the root of this worktree
            if self.other_mount_paths.contains(&child_abs_path) {
                continue;
            }

            if child_metadata.is_dir() {
                new_entries.push(Entry {
                    kind: EntryKind::PendingDir,
                    path: child_path.clone(),
                    inode: child_inode,
                    is_symlink: child_is_symlink,
                    is_ignored: None,
                });
                new_jobs.push(ScanJob {
                    abs_path: child_abs_path,
                    path: child_path,
                    scan_queue: job.scan_queue.clone(),
                });
            } else {
                new_entries.push(Entry {
                    kind: EntryKind::File(self.char_bag(&child_path)),
                    path: child_path,
                    inode: child_inode,
                    is_symlink: child_is_symlink,
                    is_ignored: None,
                });
            };
        }

        self.snapshot
            .lock()
            .populate_dir(job.path.clone(), new_entries);
        for new_job in new_jobs {
            job.scan_queue.send(new_job).unwrap();
        }

        Ok(())
    }

    fn process_events(&mut self, mut events: Vec<fsevent::Event>) -> bool {
        self.update_other_mount_paths();

        let mut snapshot = self.snapshot();
        snapshot.scan_id += 1;

        let root_abs_path = if let Ok(abs_path) = snapshot.abs_path.canonicalize() {
            abs_path
        } else {
            return false;
        };

        events.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut abs_paths = events.into_iter().map(|e| e.path).peekable();
        let (scan_queue_tx, scan_queue_rx) = crossbeam_channel::unbounded();

        while let Some(abs_path) = abs_paths.next() {
            let path = match abs_path.strip_prefix(&root_abs_path) {
                Ok(path) => Arc::from(path.to_path_buf()),
                Err(_) => {
                    log::error!(
                        "unexpected event {:?} for root path {:?}",
                        abs_path,
                        root_abs_path
                    );
                    continue;
                }
            };

            while abs_paths.peek().map_or(false, |p| p.starts_with(&abs_path)) {
                abs_paths.next();
            }

            snapshot.remove_path(&path);

            match self.fs_entry_for_path(path.clone(), &abs_path) {
                Ok(Some(fs_entry)) => {
                    let is_dir = fs_entry.is_dir();
                    snapshot.insert_entry(fs_entry);
                    if is_dir {
                        scan_queue_tx
                            .send(ScanJob {
                                abs_path,
                                path,
                                scan_queue: scan_queue_tx.clone(),
                            })
                            .unwrap();
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    // TODO - create a special 'error' entry in the entries tree to mark this
                    log::error!("error reading file on event {:?}", err);
                }
            }
        }

        *self.snapshot.lock() = snapshot;

        // Scan any directories that were created as part of this event batch.
        drop(scan_queue_tx);
        self.thread_pool.scoped(|pool| {
            for _ in 0..self.thread_pool.thread_count() {
                pool.execute(|| {
                    while let Ok(job) = scan_queue_rx.recv() {
                        if let Err(err) = self.scan_dir(&job) {
                            log::error!("error scanning {:?}: {}", job.abs_path, err);
                        }
                    }
                });
            }
        });

        self.recompute_ignore_statuses();

        true
    }

    fn recompute_ignore_statuses(&self) {
        self.compute_ignore_status_for_new_ignores();
        self.compute_ignore_status_for_new_entries();
    }

    fn compute_ignore_status_for_new_ignores(&self) {
        let mut snapshot = self.snapshot();

        let mut ignores_to_delete = Vec::new();
        let mut changed_ignore_parents = Vec::new();
        for (parent_path, (_, scan_id)) in &snapshot.ignores {
            let prev_ignore_parent = changed_ignore_parents.last();
            if *scan_id == snapshot.scan_id
                && prev_ignore_parent.map_or(true, |l| !parent_path.starts_with(l))
            {
                changed_ignore_parents.push(parent_path.clone());
            }

            let ignore_parent_exists = snapshot.entry_for_path(parent_path).is_some();
            let ignore_exists = snapshot
                .entry_for_path(parent_path.join(&*GITIGNORE))
                .is_some();
            if !ignore_parent_exists || !ignore_exists {
                ignores_to_delete.push(parent_path.clone());
            }
        }

        for parent_path in ignores_to_delete {
            snapshot.ignores.remove(&parent_path);
            self.snapshot.lock().ignores.remove(&parent_path);
        }

        let (entries_tx, entries_rx) = crossbeam_channel::unbounded();
        self.thread_pool.scoped(|scope| {
            let (edits_tx, edits_rx) = crossbeam_channel::unbounded();
            scope.execute(move || {
                let mut edits = Vec::new();
                while let Ok(edit) = edits_rx.recv() {
                    edits.push(edit);
                    while let Ok(edit) = edits_rx.try_recv() {
                        edits.push(edit);
                    }
                    self.snapshot.lock().entries.edit(mem::take(&mut edits));
                }
            });

            scope.execute(|| {
                let entries_tx = entries_tx;
                let mut cursor = snapshot.entries.cursor::<_, ()>();
                for ignore_parent_path in &changed_ignore_parents {
                    cursor.seek(&PathSearch::Exact(ignore_parent_path), SeekBias::Right);
                    while let Some(entry) = cursor.item() {
                        if entry.path().starts_with(ignore_parent_path) {
                            entries_tx.send(entry.clone()).unwrap();
                            cursor.next();
                        } else {
                            break;
                        }
                    }
                }
            });

            for _ in 0..self.thread_pool.thread_count() - 2 {
                let edits_tx = edits_tx.clone();
                scope.execute(|| {
                    let edits_tx = edits_tx;
                    while let Ok(mut entry) = entries_rx.recv() {
                        entry.set_ignored(snapshot.is_path_ignored(entry.path()).unwrap());
                        edits_tx.send(Edit::Insert(entry)).unwrap();
                    }
                });
            }
        });
    }

    fn compute_ignore_status_for_new_entries(&self) {
        let snapshot = self.snapshot.lock().clone();

        let (entries_tx, entries_rx) = crossbeam_channel::unbounded();
        self.thread_pool.scoped(|scope| {
            let (edits_tx, edits_rx) = crossbeam_channel::unbounded();
            scope.execute(move || {
                let mut edits = Vec::new();
                while let Ok(edit) = edits_rx.recv() {
                    edits.push(edit);
                    while let Ok(edit) = edits_rx.try_recv() {
                        edits.push(edit);
                    }
                    self.snapshot.lock().entries.edit(mem::take(&mut edits));
                }
            });

            scope.execute(|| {
                let entries_tx = entries_tx;
                for entry in snapshot
                    .entries
                    .filter::<_, ()>(|e| e.recompute_ignore_status)
                {
                    entries_tx.send(entry.clone()).unwrap();
                }
            });

            for _ in 0..self.thread_pool.thread_count() - 2 {
                let edits_tx = edits_tx.clone();
                scope.execute(|| {
                    let edits_tx = edits_tx;
                    while let Ok(mut entry) = entries_rx.recv() {
                        entry.set_ignored(snapshot.is_path_ignored(entry.path()).unwrap());
                        edits_tx.send(Edit::Insert(entry)).unwrap();
                    }
                });
            }
        });
    }

    fn fs_entry_for_path(&self, path: Arc<Path>, abs_path: &Path) -> Result<Option<Entry>> {
        let metadata = match fs::metadata(&abs_path) {
            Err(err) => {
                return match (err.kind(), err.raw_os_error()) {
                    (io::ErrorKind::NotFound, _) => Ok(None),
                    (io::ErrorKind::Other, Some(libc::ENOTDIR)) => Ok(None),
                    _ => Err(anyhow::Error::new(err)),
                }
            }
            Ok(metadata) => metadata,
        };
        let inode = metadata.ino();
        let is_symlink = fs::symlink_metadata(&abs_path)
            .context("failed to read symlink metadata")?
            .file_type()
            .is_symlink();

        let entry = Entry {
            kind: if metadata.file_type().is_dir() {
                EntryKind::PendingDir
            } else {
                EntryKind::File(self.char_bag(&path))
            },
            path,
            inode,
            is_symlink,
            is_ignored: None,
        };

        Ok(Some(entry))
    }

    fn char_bag(&self, path: &Path) -> CharBag {
        let mut result = self.root_char_bag;
        result.extend(path.to_string_lossy().chars());
        result
    }
}

struct ScanJob {
    abs_path: PathBuf,
    path: Arc<Path>,
    scan_queue: crossbeam_channel::Sender<ScanJob>,
}

pub trait WorktreeHandle {
    fn file(&self, path: impl AsRef<Path>, app: &AppContext) -> Result<FileHandle>;
}

impl WorktreeHandle for ModelHandle<Worktree> {
    fn file(&self, path: impl AsRef<Path>, app: &AppContext) -> Result<FileHandle> {
        self.read(app)
            .entry_for_path(&path)
            .map(|entry| FileHandle {
                worktree: self.clone(),
                path: entry.path().clone(),
            })
            .ok_or_else(|| anyhow!("path does not exist in tree"))
    }
}

pub enum FileIter<'a> {
    All(Cursor<'a, Entry, FileCount, FileCount>),
    Visible(Cursor<'a, Entry, VisibleFileCount, VisibleFileCount>),
}

impl<'a> FileIter<'a> {
    fn all(snapshot: &'a Snapshot, start: usize) -> Self {
        let mut cursor = snapshot.entries.cursor();
        cursor.seek(&FileCount(start), SeekBias::Right);
        Self::All(cursor)
    }

    fn visible(snapshot: &'a Snapshot, start: usize) -> Self {
        let mut cursor = snapshot.entries.cursor();
        cursor.seek(&VisibleFileCount(start), SeekBias::Right);
        Self::Visible(cursor)
    }

    fn next_internal(&mut self) {
        match self {
            Self::All(cursor) => {
                let ix = *cursor.start();
                cursor.seek_forward(&FileCount(ix.0 + 1), SeekBias::Right);
            }
            Self::Visible(cursor) => {
                let ix = *cursor.start();
                cursor.seek_forward(&VisibleFileCount(ix.0 + 1), SeekBias::Right);
            }
        }
    }

    fn item(&self) -> Option<&'a Entry> {
        match self {
            Self::All(cursor) => cursor.item(),
            Self::Visible(cursor) => cursor.item(),
        }
    }
}

impl<'a> Iterator for FileIter<'a> {
    type Item = &'a Entry;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entry) = self.item() {
            self.next_internal();
            Some(entry)
        } else {
            None
        }
    }
}

fn mounted_volume_paths() -> Vec<PathBuf> {
    unsafe {
        let mut stat_ptr: *mut libc::statfs = std::ptr::null_mut();
        let count = libc::getmntinfo(&mut stat_ptr as *mut _, libc::MNT_WAIT);
        if count >= 0 {
            std::slice::from_raw_parts(stat_ptr, count as usize)
                .iter()
                .map(|stat| {
                    PathBuf::from(OsStr::from_bytes(
                        CStr::from_ptr(&stat.f_mntonname[0]).to_bytes(),
                    ))
                })
                .collect()
        } else {
            panic!("failed to run getmntinfo");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Buffer;
    use crate::test::*;
    use anyhow::Result;
    use gpui::App;
    use rand::prelude::*;
    use serde_json::json;
    use std::env;
    use std::fmt::Write;
    use std::os::unix;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_populate_and_search() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "root": {
                    "apple": "",
                    "banana": {
                        "carrot": {
                            "date": "",
                            "endive": "",
                        }
                    },
                    "fennel": {
                        "grape": "",
                    }
                }
            }));

            let root_link_path = dir.path().join("root_link");
            unix::fs::symlink(&dir.path().join("root"), &root_link_path).unwrap();

            let tree = app.add_model(|ctx| Worktree::new(root_link_path, ctx));

            app.read(|ctx| tree.read(ctx).scan_complete()).await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                assert_eq!(tree.file_count(), 4);
                let results = match_paths(
                    Some(tree.snapshot()).iter(),
                    "bna",
                    false,
                    false,
                    false,
                    10,
                    ctx.thread_pool().clone(),
                )
                .into_iter()
                .map(|result| result.path)
                .collect::<Vec<Arc<Path>>>();
                assert_eq!(
                    results,
                    vec![
                        PathBuf::from("banana/carrot/date").into(),
                        PathBuf::from("banana/carrot/endive").into(),
                    ]
                );
            })
        });
    }

    #[test]
    fn test_save_file() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "file1": "the old contents",
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            app.read(|ctx| tree.read(ctx).scan_complete()).await;
            app.read(|ctx| assert_eq!(tree.read(ctx).file_count(), 1));

            let buffer = Buffer::new(1, "a line of text.\n".repeat(10 * 1024));

            let path = tree.update(&mut app, |tree, ctx| {
                let path = tree.files(0).next().unwrap().path().clone();
                assert_eq!(path.file_name().unwrap(), "file1");
                smol::block_on(tree.save(&path, buffer.snapshot(), ctx.as_ref())).unwrap();
                path
            });

            let loaded_history = app
                .read(|ctx| tree.read(ctx).load_history(&path, ctx))
                .await
                .unwrap();
            assert_eq!(loaded_history.base_text.as_ref(), buffer.text());
        });
    }

    #[test]
    fn test_rescan_simple() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                "a": {
                    "file1": "",
                },
                "b": {
                    "c": {
                        "file2": "",
                    }
                }
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            app.read(|ctx| tree.read(ctx).scan_complete()).await;
            app.read(|ctx| assert_eq!(tree.read(ctx).file_count(), 2));

            let file2 = app.read(|ctx| {
                let file2 = tree.file("b/c/file2", ctx).unwrap();
                assert_eq!(file2.path().as_ref(), Path::new("b/c/file2"));
                file2
            });

            std::fs::rename(dir.path().join("b/c"), dir.path().join("d")).unwrap();

            app.read(|ctx| tree.read(ctx).next_scan_complete()).await;

            app.read(|ctx| {
                assert_eq!(
                    tree.read(ctx)
                        .paths()
                        .map(|p| p.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    vec!["a", "a/file1", "b", "d", "d/file2"]
                )
            });

            // tree.condition(&app, move |_, _| {
            //     file2.path().as_ref() == Path::new("d/file2")
            // })
            // .await;
        });
    }

    #[test]
    fn test_rescan_with_gitignore() {
        App::test_async((), |mut app| async move {
            let dir = temp_tree(json!({
                ".git": {},
                ".gitignore": "ignored-dir\n",
                "tracked-dir": {
                    "tracked-file1": "tracked contents",
                },
                "ignored-dir": {
                    "ignored-file1": "ignored contents",
                }
            }));

            let tree = app.add_model(|ctx| Worktree::new(dir.path(), ctx));
            app.read(|ctx| tree.read(ctx).scan_complete()).await;

            app.read(|ctx| {
                let paths = tree
                    .read(ctx)
                    .paths()
                    .map(|p| p.to_str().unwrap())
                    .collect::<Vec<_>>();
                println!("paths {:?}", paths);
            });

            app.read(|ctx| {
                let tree = tree.read(ctx);
                let tracked = tree.entry_for_path("tracked-dir/tracked-file1").unwrap();
                let ignored = tree.entry_for_path("ignored-dir/ignored-file1").unwrap();
                assert_eq!(tracked.is_ignored(), Some(false));
                assert_eq!(ignored.is_ignored(), Some(true));
            });

            fs::write(dir.path().join("tracked-dir/tracked-file2"), "").unwrap();
            fs::write(dir.path().join("ignored-dir/ignored-file2"), "").unwrap();
            app.read(|ctx| tree.read(ctx).next_scan_complete()).await;
            app.read(|ctx| {
                let tree = tree.read(ctx);
                let tracked = tree.entry_for_path("tracked-dir/tracked-file2").unwrap();
                let ignored = tree.entry_for_path("ignored-dir/ignored-file2").unwrap();
                assert_eq!(tracked.is_ignored(), Some(false));
                assert_eq!(ignored.is_ignored(), Some(true));
            });
        });
    }

    #[test]
    fn test_mounted_volume_paths() {
        let paths = mounted_volume_paths();
        assert!(paths.contains(&"/".into()));
    }

    #[test]
    fn test_random() {
        let iterations = env::var("ITERATIONS")
            .map(|i| i.parse().unwrap())
            .unwrap_or(100);
        let operations = env::var("OPERATIONS")
            .map(|o| o.parse().unwrap())
            .unwrap_or(40);
        let initial_entries = env::var("INITIAL_ENTRIES")
            .map(|o| o.parse().unwrap())
            .unwrap_or(20);
        let seeds = if let Ok(seed) = env::var("SEED").map(|s| s.parse().unwrap()) {
            seed..seed + 1
        } else {
            0..iterations
        };

        for seed in seeds {
            dbg!(seed);
            let mut rng = StdRng::seed_from_u64(seed);

            let root_dir = tempdir::TempDir::new(&format!("test-{}", seed)).unwrap();
            for _ in 0..initial_entries {
                randomly_mutate_tree(root_dir.path(), 1.0, &mut rng).unwrap();
            }
            log::info!("Generated initial tree");

            let (notify_tx, _notify_rx) = smol::channel::unbounded();
            let mut scanner = BackgroundScanner::new(
                Arc::new(Mutex::new(Snapshot {
                    id: 0,
                    scan_id: 0,
                    abs_path: root_dir.path().into(),
                    entries: Default::default(),
                    ignores: Default::default(),
                    root_name_chars: Default::default(),
                })),
                notify_tx,
                0,
            );
            scanner.scan_dirs().unwrap();
            scanner.snapshot().check_invariants();

            let mut events = Vec::new();
            let mut mutations_len = operations;
            while mutations_len > 1 {
                if !events.is_empty() && rng.gen_bool(0.4) {
                    let len = rng.gen_range(0..=events.len());
                    let to_deliver = events.drain(0..len).collect::<Vec<_>>();
                    log::info!("Delivering events: {:#?}", to_deliver);
                    scanner.process_events(to_deliver);
                    scanner.snapshot().check_invariants();
                } else {
                    events.extend(randomly_mutate_tree(root_dir.path(), 0.6, &mut rng).unwrap());
                    mutations_len -= 1;
                }
            }
            log::info!("Quiescing: {:#?}", events);
            scanner.process_events(events);
            scanner.snapshot().check_invariants();

            let (notify_tx, _notify_rx) = smol::channel::unbounded();
            let new_scanner = BackgroundScanner::new(
                Arc::new(Mutex::new(Snapshot {
                    id: 0,
                    scan_id: 0,
                    abs_path: root_dir.path().into(),
                    entries: Default::default(),
                    ignores: Default::default(),
                    root_name_chars: Default::default(),
                })),
                notify_tx,
                1,
            );
            new_scanner.scan_dirs().unwrap();
            assert_eq!(scanner.snapshot().to_vec(), new_scanner.snapshot().to_vec());
        }
    }

    fn randomly_mutate_tree(
        root_path: &Path,
        insertion_probability: f64,
        rng: &mut impl Rng,
    ) -> Result<Vec<fsevent::Event>> {
        let root_path = root_path.canonicalize().unwrap();
        let (dirs, files) = read_dir_recursive(root_path.clone());

        let mut events = Vec::new();
        let mut record_event = |path: PathBuf| {
            events.push(fsevent::Event {
                event_id: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                flags: fsevent::StreamFlags::empty(),
                path,
            });
        };

        if (files.is_empty() && dirs.len() == 1) || rng.gen_bool(insertion_probability) {
            let path = dirs.choose(rng).unwrap();
            let new_path = path.join(gen_name(rng));

            if rng.gen() {
                log::info!("Creating dir {:?}", new_path.strip_prefix(root_path)?);
                fs::create_dir(&new_path)?;
            } else {
                log::info!("Creating file {:?}", new_path.strip_prefix(root_path)?);
                fs::write(&new_path, "")?;
            }
            record_event(new_path);
        } else if rng.gen_bool(0.05) {
            let ignore_dir_path = dirs.choose(rng).unwrap();
            let ignore_path = ignore_dir_path.join(&*GITIGNORE);

            let (subdirs, subfiles) = read_dir_recursive(ignore_dir_path.clone());
            let files_to_ignore = {
                let len = rng.gen_range(0..=subfiles.len());
                subfiles.choose_multiple(rng, len)
            };
            let dirs_to_ignore = {
                let len = rng.gen_range(0..subdirs.len());
                subdirs.choose_multiple(rng, len)
            };

            let mut ignore_contents = String::new();
            for path_to_ignore in files_to_ignore.chain(dirs_to_ignore) {
                write!(
                    ignore_contents,
                    "{}\n",
                    path_to_ignore
                        .strip_prefix(&ignore_dir_path)?
                        .to_str()
                        .unwrap()
                )
                .unwrap();
            }
            log::info!(
                "Creating {:?} with contents:\n{}",
                ignore_path.strip_prefix(&root_path)?,
                ignore_contents
            );
            fs::write(&ignore_path, ignore_contents).unwrap();
            record_event(ignore_path);
        } else {
            let old_path = {
                let file_path = files.choose(rng);
                let dir_path = dirs[1..].choose(rng);
                file_path.into_iter().chain(dir_path).choose(rng).unwrap()
            };

            let is_rename = rng.gen();
            if is_rename {
                let new_path_parent = dirs
                    .iter()
                    .filter(|d| !d.starts_with(old_path))
                    .choose(rng)
                    .unwrap();

                let overwrite_existing_dir =
                    !old_path.starts_with(&new_path_parent) && rng.gen_bool(0.3);
                let new_path = if overwrite_existing_dir {
                    fs::remove_dir_all(&new_path_parent).ok();
                    new_path_parent.to_path_buf()
                } else {
                    new_path_parent.join(gen_name(rng))
                };

                log::info!(
                    "Renaming {:?} to {}{:?}",
                    old_path.strip_prefix(&root_path)?,
                    if overwrite_existing_dir {
                        "overwrite "
                    } else {
                        ""
                    },
                    new_path.strip_prefix(&root_path)?
                );
                fs::rename(&old_path, &new_path)?;
                record_event(old_path.clone());
                record_event(new_path);
            } else if old_path.is_dir() {
                let (dirs, files) = read_dir_recursive(old_path.clone());

                log::info!("Deleting dir {:?}", old_path.strip_prefix(&root_path)?);
                fs::remove_dir_all(&old_path).unwrap();
                for file in files {
                    record_event(file);
                }
                for dir in dirs {
                    record_event(dir);
                }
            } else {
                log::info!("Deleting file {:?}", old_path.strip_prefix(&root_path)?);
                fs::remove_file(old_path).unwrap();
                record_event(old_path.clone());
            }
        }

        Ok(events)
    }

    fn read_dir_recursive(path: PathBuf) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let child_entries = fs::read_dir(&path).unwrap();
        let mut dirs = vec![path];
        let mut files = Vec::new();
        for child_entry in child_entries {
            let child_path = child_entry.unwrap().path();
            if child_path.is_dir() {
                let (child_dirs, child_files) = read_dir_recursive(child_path);
                dirs.extend(child_dirs);
                files.extend(child_files);
            } else {
                files.push(child_path);
            }
        }
        (dirs, files)
    }

    fn gen_name(rng: &mut impl Rng) -> String {
        (0..6)
            .map(|_| rng.sample(rand::distributions::Alphanumeric))
            .map(char::from)
            .collect()
    }

    impl Snapshot {
        fn check_invariants(&self) {
            let mut files = self.files(0);
            let mut visible_files = self.visible_files(0);
            for entry in self.entries.cursor::<(), ()>() {
                if matches!(entry.kind, EntryKind::File(_)) {
                    assert_eq!(files.next().unwrap().inode(), entry.inode);
                    if !entry.is_ignored.unwrap() {
                        assert_eq!(visible_files.next().unwrap().inode(), entry.inode);
                    }
                }
            }
            assert!(files.next().is_none());
            assert!(visible_files.next().is_none());

            for (ignore_parent_path, _) in &self.ignores {
                assert!(self.entry_for_path(ignore_parent_path).is_some());
                assert!(self
                    .entry_for_path(ignore_parent_path.join(&*GITIGNORE))
                    .is_some());
            }
        }

        fn to_vec(&self) -> Vec<(&Path, u64, bool)> {
            let mut paths = Vec::new();
            for entry in self.entries.cursor::<(), ()>() {
                paths.push((
                    entry.path().as_ref(),
                    entry.inode(),
                    entry.is_ignored().unwrap(),
                ));
            }
            paths.sort_by(|a, b| a.0.cmp(&b.0));
            paths
        }
    }
}
