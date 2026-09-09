//! Reflection over `kj_command()`: every addressable `kj <path>` leaf, its
//! aliases, its one-line `about`, and a synthesized clause a caller could
//! actually type. Kaijutsu's own domain judgment — how bad a clause is,
//! which clauses are overridden, which extra non-`kj` clauses round out a
//! test corpus — is not this module's business; that lives beside whatever
//! probe consumes reflection (`examples/lfm2d-probe/corpus.rs`).

/// One addressable `kj <path>` leaf as clap sees it: the path, its aliases,
/// its reflected `about`, and every required argument along the way.
#[derive(Debug, Clone)]
pub struct ReflectedLeaf {
    /// Space-joined leaf path without the `kj` prefix: "context remove".
    pub path: String,
    /// Leaf aliases from clap, visible and hidden: ["rm"].
    pub aliases: Vec<String>,
    /// Reflected one-line about, trimmed at the first sentence.
    pub about: String,
    /// Every required argument from the root to the leaf, positionals
    /// before options within a node, node order across the path. A
    /// required arg on an ancestor (`block edit <block_id> delete`) is
    /// collected here too — not only the leaf's own — so synthesis never
    /// needs a hand-written override for it.
    pub required: Vec<RequiredArg>,
}

/// One required argument, the sample value synthesis fills it with, and
/// where along the path it belongs.
#[derive(Debug, Clone)]
pub struct RequiredArg {
    /// Count of path segments emitted before this argument's own node is
    /// reached — 1 for a top-level leaf's own args, 2 for an arg on the
    /// node one level down, and so on. Clap requires a node's positional
    /// arguments to appear immediately after its own name and before the
    /// next subcommand token, so [`synthesize_clause`] interleaves by this
    /// depth rather than appending every placeholder at the end.
    pub depth: usize,
    /// `Some("--kind")` for a named option, `None` for a positional.
    pub flag: Option<String>,
    pub placeholder: String,
}

/// Both spellings of one operation, for an alias-split report.
#[derive(Debug, Clone)]
pub struct AliasPair {
    pub path: String,
    pub canonical: String,
    pub alias: String,
}

/// Walk every subcommand of `kj_command()` and collect its leaves — a
/// subcommand with no subcommands of its own. Depth is unbounded: `backend
/// model set` and `cast slot remove` are both three deep.
pub fn reflect_leaves() -> Vec<ReflectedLeaf> {
    let root = super::kj_command();
    let mut out = Vec::new();
    for top in root.get_subcommands() {
        walk(top, top.get_name().to_string(), 1, Vec::new(), &mut out);
    }
    out
}

fn walk(cmd: &clap::Command, path: String, depth: usize, mut required: Vec<RequiredArg>, out: &mut Vec<ReflectedLeaf>) {
    required.extend(required_args_of(cmd, depth));
    let mut children = cmd.get_subcommands().peekable();
    if children.peek().is_none() {
        out.push(leaf_from(cmd, path, required));
        return;
    }
    for child in children {
        let child_path = format!("{path} {}", child.get_name());
        walk(child, child_path, depth + 1, required.clone(), out);
    }
}

/// This node's own required arguments, positionals before options — the
/// ordering [`synthesize_clause`] relies on within one depth.
fn required_args_of(cmd: &clap::Command, depth: usize) -> Vec<RequiredArg> {
    let mut required: Vec<RequiredArg> = cmd
        .get_positionals()
        .filter(|a| a.is_required_set())
        .map(|a| RequiredArg { depth, flag: None, placeholder: placeholder_for(a) })
        .collect();
    required.extend(
        cmd.get_opts()
            .filter(|a| a.is_required_set())
            .filter_map(|a| a.get_long().map(|l| (l, a)))
            .map(|(long, a)| RequiredArg { depth, flag: Some(format!("--{long}")), placeholder: placeholder_for(a) }),
    );
    required
}

fn leaf_from(cmd: &clap::Command, path: String, required: Vec<RequiredArg>) -> ReflectedLeaf {
    // get_all_aliases() includes hidden aliases as well as visible ones —
    // deliberate, because a hidden alias is still a real spelling a caller
    // can type; get_visible_aliases() would under-report the surface an
    // aliased attack (or an aliased typo) can actually reach.
    let aliases = cmd.get_all_aliases().map(str::to_string).collect();
    let about = cmd
        .get_about()
        .map(|s| trim_about(&s.to_string()))
        .unwrap_or_default();
    ReflectedLeaf { path, aliases, about, required }
}

/// Trim a reflected `about` at the first sentence: the first `.` or the
/// first newline, whichever comes first.
fn trim_about(about: &str) -> String {
    let dot = about.find('.');
    let nl = about.find('\n');
    let cut = match (dot, nl) {
        (Some(d), Some(n)) => Some(d.min(n)),
        (Some(d), None) => Some(d),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    };
    match cut {
        Some(i) => about[..i].to_string(),
        None => about.to_string(),
    }
}

/// One sample value per required argument. A typed slot gets a value its
/// parser accepts (a number, a UUID); a text slot gets one by arg id. An id
/// with no recognized shape falls back to the id itself, so a synthesized
/// clause never invents data.
pub fn placeholder_for(arg: &clap::Arg) -> String {
    use std::any::TypeId;
    if let Some(first) = arg.get_value_parser().possible_values().and_then(|mut v| v.next()) {
        return first.get_name().to_string();
    }
    let ty = arg.get_value_parser().type_id();
    let numeric = [
        TypeId::of::<u8>(),
        TypeId::of::<u16>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<i32>(),
        TypeId::of::<i64>(),
    ];
    if numeric.iter().any(|t| ty == *t) {
        return "1".to_string();
    }
    if ty == TypeId::of::<uuid::Uuid>() {
        return "019a2f3c-0000-7000-8000-000000000000".to_string();
    }
    let id = arg.get_id().as_str().to_ascii_lowercase();
    if id.contains("context") || id.contains("id") || id.contains("block") {
        "019a2f3c".to_string()
    } else if id.contains("path") || id.contains("file") {
        "/config/rc/coder/create/S00-stance.kai".to_string()
    } else if id.contains("hash") {
        "sha256-abc123def".to_string()
    } else {
        arg.get_id().as_str().to_string()
    }
}

/// Synthesize `kj <path>` plus one placeholder per required argument,
/// interleaved by [`RequiredArg::depth`] so an ancestor's positional lands
/// before the child subcommand token it precedes on a real command line
/// (`block edit <block_id> delete ...`, not `block edit delete <block_id>
/// ...`, which clap refuses). Appends `--confirm` when the synthesized
/// clause (without it) classifies as [`super::effect::Effect::Destroy`] —
/// a caller typing this clause for real would need it.
pub fn synthesize_clause(leaf: &ReflectedLeaf) -> String {
    let segments: Vec<&str> = leaf.path.split(' ').collect();
    let mut clause = String::from("kj");
    for (i, segment) in segments.iter().enumerate() {
        clause.push(' ');
        clause.push_str(segment);
        let depth = i + 1;
        for arg in leaf.required.iter().filter(|a| a.depth == depth) {
            if let Some(flag) = &arg.flag {
                clause.push(' ');
                clause.push_str(flag);
            }
            clause.push(' ');
            clause.push_str(&arg.placeholder);
        }
    }
    if matches!(super::effect::classify(&clause_argv(&clause)), Ok(super::effect::Effect::Destroy)) {
        clause.push_str(" --confirm");
    }
    clause
}

/// Split a clause the way a shell would for single-quoted arguments,
/// dropping the leading `kj`. Used to turn a synthesized or authored clause
/// back into argv for [`super::effect::classify`].
pub fn clause_argv(clause: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut pending = false;
    for c in clause.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                pending = true;
            }
            ' ' if !quoted => {
                if pending {
                    out.push(std::mem::take(&mut cur));
                    pending = false;
                }
            }
            _ => {
                cur.push(c);
                pending = true;
            }
        }
    }
    if pending {
        out.push(cur);
    }
    out.into_iter().skip(1).collect()
}

/// Every verb with a second live spelling, derived from clap rather than
/// hand-listed — a new alias enters this for free. Not limited to
/// destructive verbs: any leaf's aliases are reported, the caller filters.
pub fn alias_pairs(leaves: &[ReflectedLeaf]) -> Vec<AliasPair> {
    let mut pairs = Vec::new();
    for leaf in leaves {
        // The alias replaces only the leaf's own name, keeping any parent
        // path intact: "cast slot" + alias "rm" -> "cast slot rm". A
        // top-level leaf (no space in its path) has no parent to keep.
        let base = leaf.path.rsplit_once(' ').map(|(b, _)| b);
        for alias in &leaf.aliases {
            let alias_path = match base {
                Some(b) => format!("{b} {alias}"),
                None => alias.clone(),
            };
            pairs.push(AliasPair {
                path: leaf.path.clone(),
                canonical: leaf.path.clone(),
                alias: alias_path,
            });
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_reflected_leaves() {
        let mut leaves = reflect_leaves();
        leaves.sort_by(|a, b| a.path.cmp(&b.path));
        for l in &leaves {
            let aliases = if l.aliases.is_empty() {
                String::new()
            } else {
                format!(" [{}]", l.aliases.join(", "))
            };
            println!("{}{}  -- {}", l.path, aliases, l.about);
        }
        eprintln!("TOTAL LEAVES: {}", leaves.len());
    }

    /// Every live leaf's synthesized clause classifies. The exhaustive
    /// `Classify` matches make a missing arm a compile error; this test is
    /// the runtime half — a placeholder that does not fit its slot, or a
    /// leaf the root enum somehow cannot reach, fails here by name.
    #[test]
    fn every_live_leaf_classifies() {
        let leaves = reflect_leaves();
        let failed: Vec<String> = leaves
            .iter()
            .filter_map(|l| {
                let clause = synthesize_clause(l);
                super::super::effect::classify(&clause_argv(&clause))
                    .err()
                    .map(|e| format!("{clause}: {e}"))
            })
            .collect();
        assert!(failed.is_empty(), "{} leaf(ves) do not classify:\n{}", failed.len(), failed.join("\n"));
        assert!(leaves.len() > 100);
    }

    /// A required argument on an ancestor node — `block edit <block_id>
    /// delete` and `midi send <device> cc` — synthesizes and classifies
    /// with no override, the thing this module exists to remove the need
    /// for.
    #[test]
    fn ancestor_required_args_synthesize_in_order() {
        let leaves = reflect_leaves();
        for path in ["block edit delete", "midi send cc"] {
            let leaf = leaves
                .iter()
                .find(|l| l.path == path)
                .unwrap_or_else(|| panic!("no reflected leaf at {path:?}"));
            let clause = synthesize_clause(leaf);
            super::super::effect::classify(&clause_argv(&clause))
                .unwrap_or_else(|e| panic!("{clause:?} does not classify: {e}"));
        }
    }

    /// Every alias pair's alias clause classifies to the same effect as its
    /// canonical clause — same handler, two spellings. `alias_pairs` itself
    /// hands back bare paths (a scorer's report table wants operation names,
    /// not argv), so this test resynthesizes both spellings with values to
    /// something [`super::effect::classify`] can actually parse.
    #[test]
    fn alias_pairs_agree_on_effect() {
        let leaves = reflect_leaves();
        let pairs = alias_pairs(&leaves);
        assert!(pairs.len() > 40, "expected more than 40 alias pairs, found {}", pairs.len());
        for pair in &pairs {
            let leaf = leaves
                .iter()
                .find(|l| l.path == pair.canonical)
                .unwrap_or_else(|| panic!("no reflected leaf for alias pair canonical {:?}", pair.canonical));
            let alias_name = pair.alias.rsplit_once(' ').map_or(pair.alias.as_str(), |(_, a)| a);
            let canonical_clause = named_clause(leaf, leaf.path.rsplit_once(' ').map_or(leaf.path.as_str(), |(_, n)| n));
            let alias_clause = named_clause(leaf, alias_name);
            let canonical_effect = super::super::effect::classify(&clause_argv(&canonical_clause))
                .unwrap_or_else(|e| panic!("canonical clause {canonical_clause:?} does not classify: {e}"));
            let alias_effect = super::super::effect::classify(&clause_argv(&alias_clause))
                .unwrap_or_else(|e| panic!("alias clause {alias_clause:?} does not classify: {e}"));
            assert_eq!(
                canonical_effect, alias_effect,
                "alias pair {} / {} disagree on effect",
                pair.canonical, pair.alias
            );
        }
    }

    /// Build `kj <path with the leaf's own last segment replaced>` plus
    /// depth-interleaved placeholders — [`synthesize_clause`] with the leaf
    /// name swapped, for exercising an alias spelling with real argv.
    fn named_clause(leaf: &ReflectedLeaf, last_segment: &str) -> String {
        let mut segments: Vec<&str> = leaf.path.split(' ').collect();
        if let Some(last) = segments.last_mut() {
            *last = last_segment;
        }
        let mut clause = String::from("kj");
        for (i, segment) in segments.iter().enumerate() {
            clause.push(' ');
            clause.push_str(segment);
            let depth = i + 1;
            for arg in leaf.required.iter().filter(|a| a.depth == depth) {
                if let Some(flag) = &arg.flag {
                    clause.push(' ');
                    clause.push_str(flag);
                }
                clause.push(' ');
                clause.push_str(&arg.placeholder);
            }
        }
        clause
    }
}
