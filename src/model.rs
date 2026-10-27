use serde::Serialize;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub allocated: u64,
    pub apparent: u64,
    pub inodes: u64,
}

impl Usage {
    pub fn bytes(self, apparent: bool) -> u64 {
        if apparent {
            self.apparent
        } else {
            self.allocated
        }
    }
    pub fn add(&mut self, other: Self) {
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.apparent = self.apparent.saturating_add(other.apparent);
        self.inodes = self.inodes.saturating_add(other.inodes);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl Filter {
    pub fn matches(self, uid: u32, gid: u32) -> bool {
        self.uid.is_none_or(|u| u == uid) && self.gid.is_none_or(|g| g == gid)
    }
    pub fn is_empty(self) -> bool {
        self.uid.is_none() && self.gid.is_none()
    }
}

#[derive(Debug)]
pub struct Node {
    pub name: OsString,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub uid: u32,
    pub gid: u32,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub shared_link: bool,
    pub own: Usage,
    pub total: Usage,
    // Only directories allocate this map. Pair keys support combined filters.
    pub owners: HashMap<(u32, u32), Usage>,
}

impl Node {
    pub fn usage(&self, filter: Filter) -> Usage {
        if filter.is_empty() {
            return self.total;
        }
        if !self.is_dir {
            return if filter.matches(self.uid, self.gid) {
                self.own
            } else {
                Usage::default()
            };
        }
        let mut result = Usage::default();
        for (&(uid, gid), &usage) in &self.owners {
            if filter.matches(uid, gid) {
                result.add(usage);
            }
        }
        result
    }
}

#[derive(Debug)]
pub struct Tree {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    pub errors: Vec<String>,
    pub error_count: u64,
    pub elapsed_seconds: f64,
}

impl Tree {
    /// Children always follow parents in the arena, so reverse order is postorder.
    pub fn aggregate(&mut self) {
        for node in &mut self.nodes {
            node.total = node.own;
            node.owners.clear();
            if node.is_dir {
                node.owners.insert((node.uid, node.gid), node.own);
            }
        }
        for id in (1..self.nodes.len()).rev() {
            let parent = self.nodes[id].parent.expect("non-root has parent");
            let (before, after) = self.nodes.split_at_mut(id);
            let child = &after[0];
            let target = &mut before[parent];
            target.total.add(child.total);
            if child.is_dir {
                for (&owner, &usage) in &child.owners {
                    target.owners.entry(owner).or_default().add(usage);
                }
            } else {
                target
                    .owners
                    .entry((child.uid, child.gid))
                    .or_default()
                    .add(child.own);
            }
        }
    }

    pub fn path(&self, mut id: usize) -> PathBuf {
        let mut parts = Vec::new();
        while let Some(parent) = self.nodes[id].parent {
            parts.push(&self.nodes[id].name);
            id = parent;
        }
        let mut path = self.root.clone();
        for part in parts.into_iter().rev() {
            path.push(part);
        }
        path
    }

    pub fn ownership(&self, id: usize, by_group: bool, filter: Filter) -> Vec<(u32, Usage)> {
        let node = &self.nodes[id];
        let mut totals: HashMap<u32, Usage> = HashMap::new();
        if node.is_dir {
            for (&(uid, gid), &usage) in &node.owners {
                if filter.matches(uid, gid) {
                    totals
                        .entry(if by_group { gid } else { uid })
                        .or_default()
                        .add(usage);
                }
            }
        } else if filter.matches(node.uid, node.gid) {
            totals.insert(if by_group { node.gid } else { node.uid }, node.own);
        }
        let mut rows: Vec<_> = totals.into_iter().collect();
        rows.sort_by_key(|&(id, _)| id);
        rows
    }
}

/// Escape invalid bytes and controls, retaining valid Unicode for terminal display.
#[cfg(unix)]
pub fn display_name(name: &OsStr) -> String {
    let mut out = String::new();
    let mut bytes = name.as_bytes();
    while !bytes.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(bytes) {
            Ok(s) => (s, 0),
            Err(e) => (
                std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap(),
                e.error_len().unwrap_or(bytes.len() - e.valid_up_to()),
            ),
        };
        for ch in valid.chars() {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                out.extend(ch.escape_default());
            } else if ch == '\\' {
                out.push_str("\\\\");
            } else {
                out.push(ch);
            }
        }
        bytes = &bytes[valid.len()..];
        for &byte in &bytes[..invalid] {
            out.push_str(&format!("\\x{byte:02x}"));
        }
        bytes = &bytes[invalid..];
    }
    out
}

#[cfg(windows)]
pub fn display_name(name: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;
    let mut out = String::new();
    for ch in char::decode_utf16(name.encode_wide()) {
        match ch {
            Ok(ch)
                if ch.is_control()
                    || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                out.extend(ch.escape_default())
            }
            Ok(ch) => out.push(ch),
            Err(error) => out.push_str(&format!("\\u{{{:04x}}}", error.unpaired_surrogate())),
        }
    }
    out
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
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
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    #[test]
    #[cfg(unix)]
    fn escapes_terminal_controls_and_invalid_bytes() {
        let name = OsString::from_vec(b"ok\x1b[31m\n\xff\\".to_vec());
        assert_eq!(display_name(&name), "ok\\u{1b}[31m\\n\\xff\\\\");
        assert_eq!(display_name(OsStr::new("α研究")), "α研究");
    }

    #[test]
    fn ownership_uses_descendants_not_directory_owner() {
        fn node(uid: u32, gid: u32, parent: Option<usize>, bytes: u64, is_dir: bool) -> Node {
            Node {
                name: "test".into(),
                parent,
                children: vec![],
                uid,
                gid,
                is_dir,
                is_symlink: false,
                shared_link: false,
                own: Usage {
                    allocated: bytes,
                    apparent: bytes,
                    inodes: 1,
                },
                total: Usage::default(),
                owners: HashMap::new(),
            }
        }
        let mut tree = Tree {
            root: ".".into(),
            errors: vec![],
            error_count: 0,
            elapsed_seconds: 0.0,
            nodes: vec![
                node(1, 10, None, 4, true),
                node(2, 20, Some(0), 100, false),
                node(3, 20, Some(0), 200, true),
                node(2, 30, Some(2), 300, false),
            ],
        };
        tree.aggregate();
        assert_eq!(tree.nodes[0].total.allocated, 604);
        assert_eq!(
            tree.nodes[0]
                .usage(Filter {
                    uid: None,
                    gid: Some(20)
                })
                .allocated,
            300
        );
        assert_eq!(
            tree.nodes[0]
                .usage(Filter {
                    uid: Some(2),
                    gid: Some(20)
                })
                .allocated,
            100
        );
        assert_eq!(
            tree.nodes[0]
                .usage(Filter {
                    uid: Some(2),
                    gid: None
                })
                .allocated,
            400
        );
        let sum: u64 = tree
            .ownership(0, true, Filter::default())
            .iter()
            .map(|(_, u)| u.allocated)
            .sum();
        assert_eq!(sum, 604);
        tree.aggregate(); // recomputation is idempotent
        assert_eq!(tree.nodes[0].total.allocated, 604);
    }
}
