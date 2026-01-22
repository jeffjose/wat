use anyhow::{Context, Result};
use chrono::{Local, Utc};
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
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
    last_commit: Option<(String, String, Vec<String>, i64)>,  // (hash, message, files, commit_timestamp)
    render_count: usize,  // For spinner animation
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
                    let files = Self::get_commit_files(r, &commit);
                    let commit_time = commit.time().seconds();
                    (short_hash, message, files, commit_time)
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
            render_count: 0,
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
                    self.update_last_commit();
                } else if path.to_string_lossy().contains("/objects/") {
                    // New git objects often means a commit happened
                    self.update_last_commit();
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

    fn update_last_commit(&mut self) {
        let Some(repo) = &self.repo else {
            return;
        };

        if let Ok(head) = repo.head() {
            if let Ok(commit) = head.peel_to_commit() {
                let hash = commit.id().to_string();
                let short_hash = &hash[..7.min(hash.len())];

                // Only update if this is a new commit
                if let Some((existing_hash, _, _, _)) = &self.last_commit {
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

                let files = Self::get_commit_files(repo, &commit);
                let commit_time = commit.time().seconds();

                self.last_commit = Some((short_hash.to_string(), message, files, commit_time));
            }
        }
    }

    fn get_commit_files(repo: &Repository, commit: &git2::Commit) -> Vec<String> {
        let mut files = Vec::new();

        // Get the commit's tree
        let Ok(tree) = commit.tree() else {
            return files;
        };

        // Get parent tree (or empty tree for first commit)
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

        // Diff between parent and this commit
        if let Ok(diff) = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None) {
            for delta in diff.deltas() {
                if let Some(path) = delta.new_file().path() {
                    files.push(path.to_string_lossy().to_string());
                }
            }
        }

        files
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

fn format_relative_time(secs: u64) -> (String, &'static str) {
    if secs < 60 {
        (format!("{}s", secs), GREEN)
    } else if secs < 3600 {
        (format!("{}m", secs / 60), YELLOW)
    } else if secs < 86400 {
        (format!("{}h", secs / 3600), DIM)
    } else {
        (format!("{}d", secs / 86400), DIM)
    }
}

fn format_diff_bar(additions: usize, deletions: usize, max_width: usize) -> String {
    if additions == 0 && deletions == 0 {
        return String::new();
    }

    let total = additions + deletions;
    let bar_width = max_width.min(10); // Max 10 chars for the bar

    let add_chars = if total > 0 {
        (additions * bar_width / total).max(if additions > 0 { 1 } else { 0 })
    } else {
        0
    };
    let del_chars = bar_width.saturating_sub(add_chars).max(if deletions > 0 { 1 } else { 0 });

    // Adjust if we went over
    let add_chars = add_chars.min(bar_width.saturating_sub(if deletions > 0 { 1 } else { 0 }));

    let add_bar: String = std::iter::repeat('+').take(add_chars).collect();
    let del_bar: String = std::iter::repeat('-').take(del_chars).collect();

    format!("{GREEN}{add_bar}{RESET}{RED}{del_bar}{RESET}")
}

fn render(state: &mut WatState) -> Result<()> {
    let mut stdout = stdout();
    let (term_width, term_height) = terminal::size().unwrap_or((80, 24));
    let width = term_width as usize;

    // Increment render count for spinner
    state.render_count = state.render_count.wrapping_add(1);

    // Move cursor home and clear screen (alternate buffer prevents flicker)
    queue!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )?;

    let mut output = String::new();

    // -----------------------------------------------------------------------
    // HEADER BAR
    // -----------------------------------------------------------------------
    let path_str = state.workdir.to_string_lossy();
    let time_str = Local::now().format("%H:%M:%S").to_string();

    // Build status with spinner (if building)
    let build_status = if !state.ignored_dirs.is_empty() {
        let spinner = ['|', '/', '-', '\\'][state.render_count % 4];
        let lang = state.ignored_dirs.keys().find_map(|name| {
            match name.as_str() {
                "target" => Some("rust"),
                "node_modules" | "dist" | ".next" | ".nuxt" => Some("js"),
                "__pycache__" | ".venv" | "venv" | ".pyc" | "site-packages" => Some("python"),
                ".gradle" | "build" => Some("java"),
                "vendor" => Some("go"),
                "_build" | "deps" => Some("elixir"),
                "zig-cache" | "zig-out" => Some("zig"),
                ".dart_tool" => Some("dart"),
                "Pods" => Some("swift"),
                _ => None,
            }
        });
        match lang {
            Some(l) => format!(" {YELLOW}[building {} {}]{RESET}", l, spinner),
            None => format!(" {YELLOW}[building {}]{RESET}", spinner),
        }
    } else {
        String::new()
    };

    let title = format!(" wat | {}", path_str);
    let status_display_len = if state.ignored_dirs.is_empty() { 0 } else { 18 }; // approx [building rust /]
    let padding = width.saturating_sub(title.len() + status_display_len + time_str.len() + 2);

    output.push_str(&format!(
        "{BOLD}{CYAN}{}{RESET}{}{}{DIM}{}{RESET}\r\n",
        title,
        build_status,
        " ".repeat(padding),
        time_str
    ));
    output.push_str(&format!("{DIM}{}{RESET}\r\n", "-".repeat(width)));

    // -----------------------------------------------------------------------
    // GIT STATUS (compact, single line)
    // -----------------------------------------------------------------------
    let (dirty, staged, untracked, total_add, total_del) = state.get_git_status();

    let mut git_line = format!("{BOLD}GIT{RESET}  ");

    if state.repo.is_none() {
        git_line.push_str(&format!("{DIM}not a repo{RESET}"));
    } else if dirty == 0 && staged == 0 && untracked == 0 {
        git_line.push_str(&format!("{GREEN}ok{RESET} {DIM}clean{RESET}"));
    } else {
        // Line changes
        if total_add > 0 || total_del > 0 {
            git_line.push_str(&format!("{GREEN}+{:<5}{RESET} {RED}-{:<5}{RESET}  ", total_add, total_del));
        }
        // File counts
        if dirty > 0 {
            git_line.push_str(&format!("{YELLOW}*{dirty}{RESET} "));
        }
        if staged > 0 {
            git_line.push_str(&format!("{GREEN}+{staged}{RESET} "));
        }
        if untracked > 0 {
            git_line.push_str(&format!("{DIM}?{untracked}{RESET} "));
        }
    }

    // Git activity indicator
    if let Some((activity, time)) = &state.last_git_activity {
        if time.elapsed() < Duration::from_secs(5) {
            git_line.push_str(&format!(" {MAGENTA}[{activity}]{RESET}"));
        }
    }

    // Pad to full width and add
    let git_visible_len = 5 + 15 + 10; // rough estimate
    let git_pad = width.saturating_sub(git_visible_len);
    output.push_str(&git_line);
    output.push_str(&format!("{}\r\n\r\n", " ".repeat(git_pad)));

    // ═══════════════════════════════════════════════════════════════════════
    // LAST COMMIT
    // ═══════════════════════════════════════════════════════════════════════
    output.push_str(&format!("{BOLD}COMMIT{RESET}\r\n"));

    if let Some((hash, message, files, commit_timestamp)) = &state.last_commit {
        let now_timestamp = Utc::now().timestamp();
        let elapsed_secs = (now_timestamp - commit_timestamp).max(0) as u64;
        let (age_str, age_color) = format_relative_time(elapsed_secs);

        // Hash and age
        output.push_str(&format!(
            "  {YELLOW}{hash}{RESET}  {age_color}{:>4}{RESET}\r\n",
            age_str
        ));

        // Message (truncated if needed)
        let max_msg_len = width.saturating_sub(4);
        let display_msg = if message.len() > max_msg_len {
            format!("{}...", &message[..max_msg_len.saturating_sub(3)])
        } else {
            message.clone()
        };
        output.push_str(&format!("  {display_msg}\r\n"));

        // Files
        if !files.is_empty() {
            output.push_str(&format!("  {DIM}"));
            let max_commit_files = 3;
            for (i, file) in files.iter().take(max_commit_files).enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                output.push_str(file);
            }
            if files.len() > max_commit_files {
                output.push_str(&format!(" +{}", files.len() - max_commit_files));
            }
            output.push_str(&format!("{RESET}\r\n"));
        }
    } else {
        output.push_str(&format!("  {DIM}--{RESET}\r\n"));
    }
    output.push_str("\r\n");

    // -----------------------------------------------------------------------
    // RECENT CHANGES
    // -----------------------------------------------------------------------
    output.push_str(&format!("{BOLD}CHANGES{RESET}\r\n"));

    if state.recent_changes.is_empty() {
        output.push_str(&format!("  {DIM}--{RESET}\r\n"));
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

        let mut files: Vec<_> = by_file.into_iter()
            // Only show files that still exist
            .filter(|(path, _change)| path.exists())
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));

        // Calculate available space for changes section
        let header_lines = 8; // Rough estimate of lines used above
        let footer_lines = 3;
        let max_files = (term_height as usize)
            .saturating_sub(header_lines + footer_lines)
            .min(15);

        for (path, change) in files.iter().take(max_files) {
            // Change type indicator
            let (indicator, ind_color) = match change.change_type {
                ChangeType::Created => ("+", GREEN),
                ChangeType::Modified => ("~", YELLOW),
                ChangeType::Deleted => ("-", RED),
            };

            let rel_path = state.relative_path(path);

            // Get diff stats for this file
            let (additions, deletions) = if let Some(stats) = state.file_stats.get(path) {
                (stats.additions, stats.deletions)
            } else {
                (0, 0)
            };

            // Build the right side: "+45 -12 ++++++----"
            let right_content = if additions > 0 || deletions > 0 {
                let bar = format_diff_bar(additions, deletions, 8);
                format!("{GREEN}+{:<4}{RESET} {RED}-{:<4}{RESET} {}", additions, deletions, bar)
            } else {
                String::new()
            };
            let right_display_len = if additions > 0 || deletions > 0 { 20 } else { 0 };

            // Calculate path truncation
            let fixed_width = 6 + right_display_len; // "  + " + right content + spacing
            let max_path_len = width.saturating_sub(fixed_width);

            let display_path = if rel_path.len() > max_path_len {
                format!("..{}", &rel_path[rel_path.len().saturating_sub(max_path_len - 2)..])
            } else {
                rel_path
            };

            output.push_str(&format!(
                "  {ind_color}{indicator}{RESET} {display_path}"
            ));

            // Right-align the stats
            let current_len = 4 + display_path.len(); // "  + " + path
            let pad = width.saturating_sub(current_len + right_display_len + 1);

            if !right_content.is_empty() {
                output.push_str(&format!("{}{}\r\n", " ".repeat(pad), right_content));
            } else {
                output.push_str(&format!("{}\r\n", " ".repeat(width.saturating_sub(current_len))));
            }
        }

        if files.len() > max_files {
            output.push_str(&format!(
                "  {DIM}.. {} more{RESET}\r\n",
                files.len() - max_files
            ));
        }
    }

    // -----------------------------------------------------------------------
    // FOOTER
    // -----------------------------------------------------------------------
    // Move to bottom of screen for status bar
    let content_lines = output.matches("\r\n").count();
    let remaining = (term_height as usize).saturating_sub(content_lines + 2);

    // Fill remaining space with blank lines (clears old content)
    let blank_line = format!("{}\r\n", " ".repeat(width));
    for _ in 0..remaining {
        output.push_str(&blank_line);
    }

    output.push_str(&format!("{DIM}{}{RESET}\r\n", "-".repeat(width)));
    let footer = format!("{DIM}q{RESET} quit");
    let footer_pad = width.saturating_sub(6);
    output.push_str(&format!("{}{}", footer, " ".repeat(footer_pad)));

    print!("{}", output);
    stdout.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.path.canonicalize().context("Invalid path")?;

    // Setup terminal with alternate screen (prevents flicker)
    let mut stdout = stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let result = run(&path, args.interval);

    // Cleanup terminal
    execute!(stdout, cursor::Show, LeaveAlternateScreen)?;
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
        render(&mut state)?;

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
