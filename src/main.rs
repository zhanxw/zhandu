use clap::{Parser, ValueEnum};
use serde::Serialize;
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
};
use zhandu::{
    model::{Filter, Tree, Usage, display_name, human},
    scan::{Progress, ScanOptions, scan},
    ui,
};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum View {
    #[default]
    Directories,
    Groups,
    Users,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Fast disk usage explorer with group and user totals",
    after_help = "Keys: arrows navigate; g/u opens owner panels; v toggles single/two panels; d toggles files; p toggles percentages; Shift+C toggles size colors; Right/Enter focuses the browser or opens a directory; Left goes to parent; Tab switches panels; r refreshes; z toggles zero usage; i shows details; Home in owners selects All; / searches the focused panel; Space marks files; x reviews deletion; c clears marks; Esc clears search, then filters; a toggles size; s sorts; e shows errors; ? help; q quits.\nExit codes: 0 complete, 1 fatal error, 2 incomplete scan/deletion, 130 cancelled."
)]
struct Args {
    /// Directory to scan (includes hidden files; root symlinks are resolved)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Scan workers (default: available CPUs, capped at 8)
    #[arg(short = 't', long, value_parser = clap::value_parser!(u16).range(1..=256))]
    threads: Option<u16>,
    /// Stay on the root filesystem
    #[arg(short = 'x', long)]
    one_file_system: bool,
    /// Skip this exact file/directory basename at every depth (repeatable)
    #[arg(long, value_name = "NAME")]
    exclude: Vec<OsString>,
    /// Start with user/group panels, or a single directory browser
    #[arg(long, value_enum, default_value = "users")]
    view: View,
    /// Filter by group name / Unix GID / Windows SID
    #[arg(short = 'g', long, value_name = "GROUP")]
    group: Option<String>,
    /// Filter by user name / Unix UID / Windows SID
    #[arg(long, value_name = "USER")]
    user: Option<String>,
    /// Show logical file sizes instead of allocated disk blocks
    #[arg(short = 'a', long)]
    apparent_size: bool,
    /// Print JSON summary, owner totals, and immediate children, without TUI
    #[arg(long)]
    json: bool,
    /// Add formatted sizes (KiB, MiB, GiB, etc.) to JSON, retaining exact byte counts
    #[arg(long, requires = "json")]
    human_readable: bool,
    /// Display Unix IDs / Windows SIDs without account-name lookups
    #[arg(long)]
    numeric_ids: bool,
    /// Disable persistent scan caching (JSON always scans fresh)
    #[arg(long, conflicts_with = "cache_db")]
    no_cache: bool,
    /// Use this SQLite cache instead of the per-user default
    #[arg(long, value_name = "PATH")]
    cache_db: Option<PathBuf>,
}

fn resolve(value: Option<&str>, group: bool) -> Result<Option<u32>, String> {
    value
        .map(|value| zhandu::platform::resolve_owner(value, group))
        .transpose()
}

/// Optional display fields keep the default JSON and numeric accounting unchanged.
#[derive(Serialize)]
struct JsonUsage {
    #[serde(flatten)]
    raw: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    allocated_human: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    apparent_human: Option<String>,
}

impl JsonUsage {
    fn new(raw: Usage, human_readable: bool) -> Self {
        Self {
            raw,
            allocated_human: human_readable.then(|| human(raw.allocated)),
            apparent_human: human_readable.then(|| human(raw.apparent)),
        }
    }

    fn bytes(&self, apparent: bool) -> u64 {
        self.raw.bytes(apparent)
    }
}

#[derive(Serialize)]
struct OwnerRow {
    id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    sid: Option<String>,
    usage: JsonUsage,
}
#[derive(Serialize)]
struct ChildRow {
    name: String,
    is_directory: bool,
    shared_link: bool,
    usage: JsonUsage,
}
#[derive(Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    complete: bool,
    elapsed_seconds: f64,
    entries: usize,
    filter_uid: Option<u32>,
    filter_gid: Option<u32>,
    total: JsonUsage,
    directory_self: JsonUsage,
    groups: Vec<OwnerRow>,
    users: Vec<OwnerRow>,
    children: Vec<ChildRow>,
    error_count: u64,
    errors: Vec<String>,
}

fn report(tree: &Tree, filter: Filter, apparent: bool, human_readable: bool) -> Report {
    let owners = |by_group| {
        let mut rows: Vec<_> = tree
            .ownership(0, by_group, filter)
            .into_iter()
            .map(|(id, usage)| OwnerRow {
                id,
                sid: zhandu::platform::owner_identifier(id),
                usage: JsonUsage::new(usage, human_readable),
            })
            .collect();
        rows.sort_by(|a, b| {
            b.usage
                .bytes(apparent)
                .cmp(&a.usage.bytes(apparent))
                .then(a.id.cmp(&b.id))
        });
        rows
    };
    let mut children: Vec<_> = tree.nodes[0]
        .children
        .iter()
        .map(|&id| {
            let node = &tree.nodes[id];
            ChildRow {
                name: display_name(&node.name),
                is_directory: node.is_dir,
                shared_link: node.shared_link,
                usage: JsonUsage::new(node.usage(filter), human_readable),
            }
        })
        .filter(|row| filter.is_empty() || row.usage.raw.inodes > 0)
        .collect();
    children.sort_by(|a, b| {
        b.usage
            .bytes(apparent)
            .cmp(&a.usage.bytes(apparent))
            .then(a.name.cmp(&b.name))
    });
    let root = &tree.nodes[0];
    Report {
        schema_version: 1,
        root: display_name(tree.root.as_os_str()),
        complete: tree.error_count == 0,
        elapsed_seconds: tree.elapsed_seconds,
        entries: tree.nodes.len(),
        filter_uid: filter.uid,
        filter_gid: filter.gid,
        total: JsonUsage::new(root.usage(filter), human_readable),
        directory_self: JsonUsage::new(
            if filter.matches(root.uid, root.gid) {
                root.own
            } else {
                Usage::default()
            },
            human_readable,
        ),
        groups: owners(true),
        users: owners(false),
        children,
        error_count: tree.error_count,
        errors: tree.errors.clone(),
    }
}

fn run(args: Args) -> Result<i32, Box<dyn std::error::Error>> {
    let filter = Filter {
        uid: resolve(args.user.as_deref(), false)?,
        gid: resolve(args.group.as_deref(), true)?,
    };
    let options = ScanOptions {
        path: args.path,
        threads: args
            .threads
            .map(usize::from)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8))),
        one_file_system: args.one_file_system,
        excludes: args.exclude,
    };
    let progress = Arc::new(Progress::default());
    let signal_progress = progress.clone();
    ctrlc::set_handler(move || signal_progress.cancel())?;
    if args.json {
        let tree = match scan(options, progress.clone()) {
            Ok(tree) => tree,
            Err(_) if progress.is_cancelled() => return Ok(130),
            Err(error) => return Err(error.into()),
        };
        if progress.is_cancelled() {
            return Ok(130);
        }
        let mut output = io::BufWriter::new(io::stdout().lock());
        serde_json::to_writer_pretty(
            &mut output,
            &report(&tree, filter, args.apparent_size, args.human_readable),
        )?;
        writeln!(output)?;
        output.flush()?;
        Ok(if tree.error_count == 0 { 0 } else { 2 })
    } else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(
                "interactive mode needs a terminal; use --json for redirected output".into(),
            );
        }
        let view = match args.view {
            View::Directories => ui::View::Directories,
            View::Groups => ui::View::Groups,
            View::Users => ui::View::Users,
        };
        Ok(ui::run(
            options,
            progress,
            filter,
            view,
            args.apparent_size,
            args.numeric_ids,
            zhandu::cache::CacheConfig {
                enabled: !args.no_cache,
                path: args.cache_db,
            },
        )?)
    }
}

fn main() {
    let code = match run(Args::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("zhandu: {}", display_name(error.to_string().as_ref()));
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_size_labels_cover_zero_unit_boundaries_and_large_values() {
        for (bytes, label) in [
            (0, "0.0 B"),
            (1023, "1023.0 B"),
            (1024, "1.0 KiB"),
            (1 << 20, "1.0 MiB"),
            (1 << 30, "1.0 GiB"),
            (1 << 40, "1.0 TiB"),
            (u64::MAX, "16.0 EiB"),
        ] {
            let usage = Usage {
                allocated: bytes,
                apparent: bytes,
                inodes: 1,
            };
            let output = serde_json::to_value(JsonUsage::new(usage, true)).unwrap();
            assert_eq!(output["allocated"].as_u64(), Some(bytes));
            assert_eq!(output["allocated_human"], label);
            assert_eq!(output["apparent_human"], label);
        }
    }

    #[test]
    fn default_is_two_panels_and_legacy_single_startup_is_explicit() {
        assert!(matches!(Args::parse_from(["zhandu"]).view, View::Users));
        assert!(matches!(
            Args::parse_from(["zhandu", "--view", "directories"]).view,
            View::Directories
        ));
    }
}
