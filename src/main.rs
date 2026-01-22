use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use git2::{DiffOptions, Repository, StatusOptions};
use notify::{Config, Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

// ANSI color codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

#[derive(Parser, Debug)]
#[command(name = "wat")]
#[command(about = "Watch what LLM agents are doing to your files", long_about = None)]
struct Args {
    /// Path to watch (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Refresh interval in milliseconds
    #[arg(short, long, default_value = "500")]
    interval: u64,
}

#[derive(Debug, Clone)]
struct FileChange {
    path: PathBuf,
    timestamp: Instant,
    change_type: ChangeType,
}

#[derive(Debug, Clone)]
enum ChangeType {
    Created,
    Modified,
    Deleted,
}

struct WatState {
    repo: Option<Repository>,
    recent_changes: Vec<FileChange>,
    file_stats: HashMap<PathBuf, FileStats>,
    last_git_activity: Option<(String, Instant)>,
    workdir: PathBuf,
    ignored_dirs: HashMap<String, Instant>,  // Track activity in ignored directories
    last_commit: Option<(String, String, Instant)>,  // (hash, message, time)
}

#[derive(Debug, Clone, Default)]
struct FileStats {
    additions: usize,
    deletions: usize,
    last_modified: Option<Instant>,
}

impl WatState {
    fn new(path: &Path) -> Self {
        let repo = Repository::discover(path).ok();
        let workdir = repo
            .as_ref()
            .and_then(|r| r.workdir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| path.to_path_buf());

        // Get initial last commit
        let last_commit = repo.as_ref().and_then(|r| {
            r.head().ok().and_then(|head| {
                head.peel_to_commit().ok().map(|commit| {
                    let hash = commit.id().to_string();
                    let short_hash = hash[..7.min(hash.len())].to_string();
                    let message = commit
                        .message()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    (short_hash, message, Instant::now() - Duration::from_secs(3600)) // Mark as old
                })
            })
        });

        Self {
            repo,
            recent_changes: Vec::new(),
            file_stats: HashMap::new(),
            last_git_activity: None,
            workdir,
            ignored_dirs: HashMap::new(),
            last_commit,
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        let Some(repo) = &self.repo else {
            return false;
        };
        let relative = path.strip_prefix(&self.workdir).unwrap_or(path);
        repo.status_should_ignore(relative).unwrap_or(false)
    }

    fn get_ignored_root(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.workdir).unwrap_or(path);
        let components: Vec<_> = relative.components().collect();
        if components.is_empty() {
            return None;
        }
        // Return the first directory component (e.g., "target", "node_modules")
        Some(components[0].as_os_str().to_string_lossy().to_string())
    }

    fn process_event(&mut self, event: NotifyEvent) {
        let now = Instant::now();

        for path in event.paths {
            // Skip .git directory internals but track git operations
            if path.to_string_lossy().contains(".git") {
                if path.to_string_lossy().contains("index.lock") {
                    self.last_git_activity = Some(("staging".to_string(), now));
                } else if path.to_string_lossy().contains("COMMIT_EDITMSG") {
                    self.last_git_activity = Some(("committing".to_string(), now));
                } else if path.to_string_lossy().ends_with(".git/HEAD")
                    || path.to_string_lossy().contains("refs/heads")
                {
                    self.last_git_activity = Some(("branch change".to_string(), now));
                    // Check for new commit
                    self.update_last_commit(now);
                } else if path.to_string_lossy().contains("/objects/") {
                    // New git objects often means a commit happened
                    self.update_last_commit(now);
                }
                continue;
            }

            let change_type = match event.kind {
                notify::EventKind::Create(_) => ChangeType::Created,
                notify::EventKind::Modify(_) => ChangeType::Modified,
                notify::EventKind::Remove(_) => ChangeType::Deleted,
                _ => continue,
            };

            // Check if file is gitignored
            if self.is_ignored(&path) {
                if let Some(root_dir) = self.get_ignored_root(&path) {
                    self.ignored_dirs.insert(root_dir, now);
                }
                continue;
            }

            // Update file stats
            if let Some(repo) = &self.repo {
                if let Ok(diff_stats) = self.get_file_diff_stats(repo, &path) {
                    let stats = self.file_stats.entry(path.clone()).or_default();
                    stats.additions = diff_stats.0;
                    stats.deletions = diff_stats.1;
                    stats.last_modified = Some(now);
                }
            }

            self.recent_changes.push(FileChange {
                path,
                timestamp: now,
                change_type,
            });
        }

        // Keep only recent changes (last 30 seconds)
        self.recent_changes
            .retain(|c| c.timestamp.elapsed() < Duration::from_secs(30));

        // Keep only recent file stats (last 60 seconds)
        self.file_stats
            .retain(|_, v| v.last_modified.map_or(false, |t| t.elapsed() < Duration::from_secs(60)));

        // Keep only recent ignored dir activity (last 10 seconds)
        self.ignored_dirs
            .retain(|_, t| t.elapsed() < Duration::from_secs(10));
    }

    fn get_file_diff_stats(&self, repo: &Repository, path: &Path) -> Result<(usize, usize)> {
        let workdir = repo.workdir().context("No workdir")?;
        let relative = path.strip_prefix(workdir).unwrap_or(path);

        let mut opts = DiffOptions::new();
        opts.pathspec(relative);

        let diff = repo.diff_index_to_workdir(None, Some(&mut opts))?;
        let stats = diff.stats()?;

        Ok((stats.insertions(), stats.deletions()))
    }

    fn get_git_status(&self) -> (i32, i32, i32, usize, usize) {
        let Some(repo) = &self.repo else {
            return (0, 0, 0, 0, 0);
        };

        let mut opts = StatusOptions::new();
        opts.include_untracked(true);

        let mut dirty = 0;
        let mut staged = 0;
        let mut untracked = 0;

        if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
            for entry in statuses.iter() {
                let status = entry.status();
                if status.is_wt_modified() || status.is_wt_deleted() {
                    dirty += 1;
                }
                if status.is_index_modified() || status.is_index_new() || status.is_index_deleted() {
                    staged += 1;
                }
                if status.is_wt_new() && !status.is_index_new() {
                    untracked += 1;
                }
            }
        }

        // Get total diff stats
        let (total_add, total_del) = self.get_total_diff_stats();

        (dirty, staged, untracked, total_add, total_del)
    }

    fn get_total_diff_stats(&self) -> (usize, usize) {
        let Some(repo) = &self.repo else {
            return (0, 0);
        };

        // Staged changes (index vs HEAD)
        let mut staged_add = 0;
        let mut staged_del = 0;
        if let Ok(head) = repo.head() {
            if let Ok(tree) = head.peel_to_tree() {
                if let Ok(diff) = repo.diff_tree_to_index(Some(&tree), None, None) {
                    if let Ok(stats) = diff.stats() {
                        staged_add = stats.insertions();
                        staged_del = stats.deletions();
                    }
                }
            }
        }

        // Unstaged changes (workdir vs index)
        let mut unstaged_add = 0;
        let mut unstaged_del = 0;
        if let Ok(diff) = repo.diff_index_to_workdir(None, None) {
            if let Ok(stats) = diff.stats() {
                unstaged_add = stats.insertions();
                unstaged_del = stats.deletions();
            }
        }

        (staged_add + unstaged_add, staged_del + unstaged_del)
    }

    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.workdir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    fn update_last_commit(&mut self, now: Instant) {
        let Some(repo) = &self.repo else {
            return;
        };

        if let Ok(head) = repo.head() {
            if let Ok(commit) = head.peel_to_commit() {
                let hash = commit.id().to_string();
                let short_hash = &hash[..7.min(hash.len())];

                // Only update if this is a new commit
                if let Some((existing_hash, _, _)) = &self.last_commit {
                    if existing_hash == short_hash {
                        return;
                    }
                }

                let message = commit
                    .message()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();

                self.last_commit = Some((short_hash.to_string(), message, now));
            }
        }
    }
}

fn setup_watcher(path: &Path) -> Result<(RecommendedWatcher, Receiver<notify::Result<NotifyEvent>>)> {
    let (tx, rx) = channel();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default().with_poll_interval(Duration::from_millis(100)),
    )?;

    watcher.watch(path, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}

fn render(state: &WatState) -> Result<()> {
    let mut stdout = stdout();
    let (term_width, term_height) = terminal::size().unwrap_or((80, 24));

    // Clear and move to top
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    let mut output = String::new();

    // Header: wat - /path/to/directory $time
    output.push_str(&format!(
        "{BOLD}{CYAN}wat{RESET} {DIM}-{RESET} {}  {DIM}{}{RESET}\r\n",
        state.workdir.display(),
        Local::now().format("%H:%M:%S")
    ));

    // Git status line
    let (dirty, staged, untracked, total_add, total_del) = state.get_git_status();

    if state.repo.is_none() {
        output.push_str(&format!("{DIM}not a git repo{RESET}"));
    } else if dirty == 0 && staged == 0 && untracked == 0 {
        // Clean - show nothing or minimal indicator
        output.push_str(&format!("{DIM}clean{RESET}"));
    } else {
        // Show +lines -lines in colors first
        if total_add > 0 || total_del > 0 {
            output.push_str(&format!("{GREEN}+{total_add}{RESET} {RED}-{total_del}{RESET}"));
        }

        // Then file counts: dirty:N staged:N ?:N
        let mut parts = Vec::new();
        if dirty > 0 {
            parts.push(format!("{YELLOW}dirty:{dirty}{RESET}"));
        }
        if staged > 0 {
            parts.push(format!("{GREEN}staged:{staged}{RESET}"));
        }
        if untracked > 0 {
            parts.push(format!("{DIM}?:{untracked}{RESET}"));
        }
        if !parts.is_empty() {
            if total_add > 0 || total_del > 0 {
                output.push_str("  ");
            }
            output.push_str(&parts.join(" "));
        }
    }

    // Git activity indicator
    if let Some((activity, time)) = &state.last_git_activity {
        if time.elapsed() < Duration::from_secs(5) {
            output.push_str(&format!("  {MAGENTA}{activity}{RESET}"));
        }
    }
    output.push_str("\r\n");

    // Show last commit (recent commits within 60 seconds, or always show the latest)
    if let Some((hash, message, time)) = &state.last_commit {
        let elapsed = time.elapsed().as_secs();
        let age_indicator = if elapsed < 60 {
            format!("{GREEN}{elapsed}s ago{RESET}")
        } else {
            format!("{DIM}{hash}{RESET}")
        };
        // Truncate message if too long
        let max_msg_len = (term_width as usize).saturating_sub(25);
        let display_msg = if message.len() > max_msg_len {
            format!("{}...", &message[..max_msg_len.saturating_sub(3)])
        } else {
            message.clone()
        };
        output.push_str(&format!(
            "{DIM}last commit:{RESET} {age_indicator} {display_msg}\r\n"
        ));
    }
    output.push_str("\r\n");

    // Recent changes - no header, just show content
    if state.recent_changes.is_empty() {
        output.push_str(&format!("{DIM}Nothing{RESET}\r\n"));
    } else {
        // Group by file and show most recent
        let mut by_file: HashMap<PathBuf, &FileChange> = HashMap::new();
        for change in &state.recent_changes {
            by_file
                .entry(change.path.clone())
                .and_modify(|existing| {
                    if change.timestamp > existing.timestamp {
                        *existing = change;
                    }
                })
                .or_insert(change);
        }

        let mut files: Vec<_> = by_file.into_iter().collect();
        // Sort by path for consistent ordering (no jumping around)
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let max_files = (term_height as usize).saturating_sub(8).min(20);

        for (path, change) in files.iter().take(max_files) {
            let elapsed = change.timestamp.elapsed().as_secs();

            // Age color
            let age_color = if elapsed < 3 {
                GREEN
            } else if elapsed < 10 {
                YELLOW
            } else {
                DIM
            };

            // Change type indicator
            let (indicator, ind_color) = match change.change_type {
                ChangeType::Created => ("+", GREEN),
                ChangeType::Modified => ("~", YELLOW),
                ChangeType::Deleted => ("-", RED),
            };

            let rel_path = state.relative_path(path);

            // Truncate path if too long
            let max_path_len = (term_width as usize).saturating_sub(25);
            let display_path = if rel_path.len() > max_path_len {
                format!("...{}", &rel_path[rel_path.len() - max_path_len + 3..])
            } else {
                rel_path
            };

            output.push_str(&format!(
                "  {age_color}{:>2}s{RESET} {ind_color}{indicator}{RESET} {display_path}",
                elapsed
            ));

            // Show diff stats if available
            if let Some(stats) = state.file_stats.get(path) {
                if stats.additions > 0 || stats.deletions > 0 {
                    output.push_str(&format!(
                        " {GREEN}+{}{RESET} {RED}-{}{RESET}",
                        stats.additions, stats.deletions
                    ));
                }
            }

            output.push_str("\r\n");
        }

        if files.len() > max_files {
            output.push_str(&format!(
                "  {DIM}... and {} more{RESET}\r\n",
                files.len() - max_files
            ));
        }
    }

    // Show ignored directory activity
    if !state.ignored_dirs.is_empty() {
        let mut dirs: Vec<_> = state.ignored_dirs.iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(a.1));
        let dir_names: Vec<_> = dirs.iter().map(|(name, _)| format!("{}/", name)).collect();
        output.push_str(&format!(
            "\r\n{DIM}ignored: {}{RESET}\r\n",
            dir_names.join(" ")
        ));
    }

    // Footer
    output.push_str(&format!("\r\n{DIM}q to quit{RESET}"));

    print!("{}", output);
    stdout.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.path.canonicalize().context("Invalid path")?;

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, cursor::Hide)?;

    let result = run(&path, args.interval);

    // Cleanup terminal
    execute!(
        stdout,
        cursor::Show,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    terminal::disable_raw_mode()?;

    result
}

fn run(path: &Path, interval: u64) -> Result<()> {
    let (_watcher, rx) = setup_watcher(path)?;
    let mut state = WatState::new(path);

    loop {
        // Process file events
        while let Ok(event) = rx.try_recv() {
            if let Ok(e) = event {
                state.process_event(e);
            }
        }

        // Render
        render(&state)?;

        // Check for quit
        if event::poll(Duration::from_millis(interval))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
            }
        }
    }

    Ok(())
}
