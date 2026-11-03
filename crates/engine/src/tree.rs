//! Who is whose parent, where each branch was cut, and which node the next answer reads.
//!
//! Branching made siblings and nothing said who was whose parent: the sidebar drew a branch as an
//! unrelated row, and pi's own tree — which is the thing that decides what its model was told —
//! was read for nothing but a leaf id during a rewind. One tree has to be readable no matter which
//! core made the branch, so this module reads both records and says where they disagree.
//!
//! Two trees, and they are not the same tree:
//!
//! * **Conversations.** A session row carries the conversation it was cut out of and the row it was
//!   cut at. A row with no parent is a **root** — a fact, drawn as the start of its own tree, not
//!   as "unknown".
//! * **Nodes inside one conversation.** The Rust core's node list is its message rows, linked by
//!   `parent_row`; the pi core's is pi's entry tree. They are matched node-by-node, and a node one
//!   side has that the other does not is a **line in the tree**, not a silent omission. That is the
//!   honest shape of "the record and the model's memory are two ledgers": they usually agree, and
//!   when they don't the disagreement has to be readable from the app instead of inferred from an
//!   answer that behaved strangely.
//!
//! What moving the leaf is, and what it is not: it changes what the next answer reads. It copies no
//! rows (that is [`crate::branch`]) and deletes none (that is [`crate::edit`]'s rewind). Rows left
//! on another branch stay in the record and are counted here, so the transcript can say how much of
//! a conversation it is not showing rather than leaving the person to wonder where it went.
//!
//! The leaf rule is the same on both cores, and pi's own navigation set it: navigating to one of
//! the person's messages puts the leaf at that message's parent and hands the words back to be said
//! again; navigating to an answer puts the leaf at that answer. One sentence, one cut, two memories.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::HostOps;
use crate::store::{Session, StoreOps};

/// How many lines of disagreement the tree quotes. A replaced file can hold hundreds of entries
/// nobody answers to; a wall of quotes is not a report. The count is stated beside the lines.
pub const DRIFT_LINES: usize = 6;

/// How many nodes one tree card lists. A conversation with a thousand rows is not a card anybody
/// can read, and a silently truncated list reads like a conversation with fewer nodes than it has —
/// so the read carries [`Tree::nodes_hidden`] and the card says what is not on it.
///
/// The branch the next answer reads is always listed in full; what gets dropped is always off-path,
/// because the sort puts the branch first.
pub const TREE_NODES: usize = 120;

/// A short, single-line version of a node, so the tree can name it without pasting a turn.
pub fn quote(s: &str) -> String {
    const MAX: usize = 70;
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("{head}…")
}

/// One conversation in the lineage tree.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationNode {
    pub session: i64,
    pub title: String,
    /// The conversation this one was cut out of. `None` is a root.
    pub parent: Option<i64>,
    /// `user` / `assistant`, or empty for a root — which is what tells a reader whether the branch
    /// holds the message it was cut at or only what came before it.
    pub branch_role: String,
    /// The parent's row the cut was made at. `None` on a child means it was branched before the cut
    /// was recorded, and `branch_quote` says so rather than inventing a row.
    pub branch_row: Option<i64>,
    /// What was at the cut, in the parent's own words — the line that tells two siblings apart.
    pub branch_quote: String,
    /// The conversation this window is looking at.
    pub current: bool,
    /// A parent that is no longer in the record. Deleting a conversation does not re-parent its
    /// children: a branch keeps its own history, and the tree names the gap instead of dropping the
    /// child or pretending the parent is there.
    pub parent_gone: bool,
    /// Rows the next answer reads here, and how many sit on branches it does not read.
    pub rows: usize,
    pub off_branch: usize,
    /// Depth from the root of this tree, for indenting.
    pub depth: usize,
    pub children: usize,
}

/// One node inside one conversation: a stored row, a pi entry, or both.
#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    /// The cwoffee row this node answers to, if it answers to one.
    pub row: Option<i64>,
    /// pi's entry id, if pi has an entry for it.
    pub pi_entry: Option<String>,
    pub role: String,
    pub quote: String,
    /// The node's words in full.
    ///
    /// Not decoration. Moving the leaf onto this node has to prove the person meant *this* row, and
    /// the proof is the client sending back the text it painted — the same discipline `crate::branch`
    /// uses. A card that only showed a 70-character quote could not make that proof, and a leaf switch
    /// addressed by id alone is a switch addressed by whatever the row id happened to be.
    pub content: String,
    /// On the path the next answer reads.
    pub on_branch: bool,
    /// The leaf: the node the **record's** next answer reads through.
    pub current: bool,
    /// Where pi's own thread is. On the pi core this can be a different node from `current`, and the
    /// two marks with no line about their disagreement would read as one answer instead of two
    /// records that have come apart.
    pub pi_current: bool,
    /// Depth from the root of the node list, for indenting.
    pub depth: usize,
    /// A node one record has and the other does not.
    pub drift: bool,
}

/// What pi answered about its own tree, kept beside the read so a check can tell "pi was asked"
/// from "we guessed from the file".
#[derive(Debug, Clone, Serialize, Default)]
pub struct PiRead {
    /// True when the live model process answered `get_tree`. False means there was no child to ask
    /// (or it holds another conversation), and then the record's own node list is all there is.
    pub consulted: bool,
    pub entries: usize,
    pub leaf_entry: Option<String>,
    pub file: String,
    /// Why pi could not be asked, when it could not be.
    pub why: String,
}

/// The whole read.
#[derive(Debug, Clone, Serialize)]
pub struct Tree {
    pub session: i64,
    pub core: String,
    /// Ancestors root→self, then this conversation's descendants: one flat list carrying `depth`,
    /// because the row menu draws an indented list rather than a diagram.
    pub lineage: Vec<ConversationNode>,
    pub nodes: Vec<TreeNode>,
    /// Nodes this conversation holds that the card did not list. Always off-path ones: the branch
    /// the next answer reads is never the part that gets dropped.
    pub nodes_hidden: usize,
    /// Rows this conversation holds that the next answer does not read.
    pub off_branch: usize,
    /// The row the next answer reads through. `None` is the newest row.
    pub leaf: Option<i64>,
    pub pi: PiRead,
    /// Lines of disagreement. Empty means the two records answer to each other, which is the usual
    /// case and the one worth being able to assert.
    pub drift: Vec<String>,
    pub drift_count: usize,
    pub note: String,
}

/// One entry of pi's thread, flattened out of `get_tree`'s nesting.
#[derive(Debug, Clone)]
pub struct PiNode {
    pub entry: String,
    pub parent: Option<String>,
    /// pi's own entry type: `message`, `compaction`, `model_change`, `thinking_level_change`, …
    pub kind: String,
    /// `user` / `assistant` / `system` / `toolResult` for a message entry, else the kind.
    pub role: String,
    pub text: String,
    /// On the path from pi's leaf to its root — i.e. what its model was told.
    pub on_branch: bool,
}

/// pi's answer about the thread it is holding.
#[derive(Debug, Clone, Default)]
pub struct PiTree {
    pub nodes: Vec<PiNode>,
    pub leaf: Option<String>,
    pub file: String,
}

/// Words compared the way the branch check compares them: pi's copy of a message and the record's
/// can differ in whitespace and still be the same message.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text of one pi entry, in the shape pi's `get_tree` hands it.
///
/// A message entry's content is a string or a list of parts; a tool result's parts are text or
/// images. Whatever comes back is what the entry is quoted by, and an entry with no text quoted as
/// empty is honest — a model change has no words, and inventing some would put a sentence on a node
/// that never had one.
fn entry_text(entry: &serde_json::Value) -> String {
    let content = entry.get("message").and_then(|m| m.get("content"));
    if let Some(s) = content.and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(parts) = content.and_then(Value::as_array) {
        let mut out = String::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        return out;
    }
    // pi's own entries carry their subject in a field of their own; quoting it is better than an
    // empty line, because the tree shows the node either way.
    for key in ["summary", "label", "modelId", "model", "thinkingLevel", "name"] {
        if let Some(s) = entry.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

/// Turn pi's `get_entries` answer into parent-linked nodes, and mark the path its leaf sits on —
/// the path its model was told.
///
/// pi answers a thread as a **flat list with `parentId`**, in file order, plus a `leafId`. File order
/// is not conversation order: once anything has been forked, a branch written last hangs off an entry
/// written first, so the path pi builds context from — it walks leaf to root — is not the path the
/// list reads top-down. The branch is therefore marked by walking **up from `leafId`**, and a node
/// that is not on it is reported as off-path rather than left out.
///
/// `get_tree` is the other answer pi gives, and it is not usable here: it returns a defensive copy of
/// the tree as nested `children` arrays, one level per entry, so a sixty-message conversation is over
/// a hundred levels deep and the bridge's JSONL reader rejects the whole record at serde_json's
/// recursion limit — the tree read came back as "unparseable record from pi" and the card drew the
/// record alone. Flat entries plus a Rust-side walk has no depth to overflow.
pub fn pi_entries_from_json(
    entries: &serde_json::Value,
    leaf: Option<&str>,
    file: &str,
) -> PiTree {
    let mut nodes: Vec<PiNode> = Vec::new();
    let list = match entries.as_array() {
        Some(l) => l,
        None => &Vec::new(),
    };
    for e in list {
        let id = e.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let kind = e.get("type").and_then(Value::as_str).unwrap_or("entry").to_string();
        let role = e
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .unwrap_or(&kind)
            .to_string();
        let parent = e.get("parentId").and_then(Value::as_str).map(str::to_string);
        nodes.push(PiNode {
            entry: id,
            parent,
            kind,
            role,
            text: entry_text(e),
            on_branch: false,
        });
    }

    // Leaf upward, so the marked path is the one the model read.
    let by_id: std::collections::HashMap<String, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.entry.clone(), i)).collect();
    let mut cur = leaf.map(str::to_string);

    let mut guard = 0usize;
    while let Some(id) = cur {
        guard += 1;
        if guard > nodes.len() + 1 {
            break;
        }
        match by_id.get(&id) {
            Some(i) => {
                nodes[*i].on_branch = true;
                cur = nodes[*i].parent.clone();
            }
            None => break,
        }
    }
    PiTree {
        nodes,
        leaf: leaf.map(str::to_string),
        file: file.to_string(),
    }
}

/// Is this pi entry something the record is expected to answer to?
///
/// pi's own writing — the entry that recorded the model, the thinking level, a fold, a label, and the
/// **system prompt** it assembled for the request — is pi's, not the person's. The record holds user
/// messages, answers and tool results; it never stores the prompt the engine built, so calling a
/// system entry a disagreement would cry wolf on every conversation on this core. Showing it as
/// nothing would hide the shape of the thread, so it is drawn as a node and not counted.
fn pi_entry_is_mine(kind: &str, role: &str) -> bool {
    kind == "message" && matches!(role, "user" | "assistant" | "toolResult" | "tool")
}

/// A message node pi has that no row answers to, said as a line of the tree.
fn drift_line_pi(n: &PiNode) -> String {
    let what = match n.role.as_str() {
        "user" => "a message of yours",
        "assistant" => "an answer",
        "system" => "its own prompt",
        "toolResult" => "a tool result",
        other => other,
    };
    format!(
        "pi's thread holds {what} the record has no row for — {}{}",
        quote(&n.text),
        if n.on_branch { "" } else { " (on a branch that is not being read)" }
    )
}

/// A row the record holds that pi's thread has no entry for.
fn drift_line_row(role: &str, content: &str) -> String {
    format!(
        "the record holds a {} pi's thread has no entry for — {}",
        if role == "user" { "message of yours" } else { role },
        quote(content)
    )
}

/// Ancestor chain root→self, then the descendants of self, depth-first.
///
/// Bounded, and the bound is said out loud: a conversation branched a hundred times is a tree the
/// row menu cannot draw, and a silently truncated list reads like a conversation with fewer
/// branches than it has.
fn lineage_of(store: &dyn StoreOps, session: i64, limit: usize) -> Result<(Vec<ConversationNode>, usize), String> {
    let me = store
        .session(session)
        .map_err(|e| format!("conversation {session} could not be read ({e})"))?
        .ok_or_else(|| format!("conversation {session} is not in the record, so it has no tree"))?;

    // Up, from self to the root. A parent id that answers to no row is a conversation that was
    // deleted; the walk stops there and the child's own node says so.
    let mut up: Vec<Session> = vec![me.clone()];
    let mut parent_gone = false;
    let mut cur = me.parent;
    let mut guard = 0usize;
    while let Some(p) = cur {
        guard += 1;
        if guard > 200 {
            // A cycle in the parent column is corruption, not a lineage. Stop rather than spin,
            // and say so in the note below.
            return Err(format!(
                "conversation {session} has a parent chain that never ends, which is not a lineage; \
                 the tree was not read"
            ));
        }
        match store.session(p).map_err(|e| e.to_string())? {
            Some(row) => {
                cur = row.parent;
                up.push(row);
            }
            None => {
                parent_gone = true;
                break;
            }
        }
    }
    up.reverse(); // root first

    let mut out: Vec<ConversationNode> = Vec::new();
    let mut dropped = 0usize;
    for (depth, row) in up.iter().enumerate() {
        let kids = store.children_of(row.id).map_err(|e| e.to_string())?;
        out.push(ConversationNode {
            rows: store.branch_chain(row.id).map(|c| c.len()).unwrap_or(0),
            off_branch: store.off_branch_rows(row.id).unwrap_or(0),
            children: kids.len(),
            depth,
            current: row.id == session,
            parent_gone: row.id == me.id && parent_gone,
            branch_quote: quote_of_cut(store, row)?,
            session: row.id,
            title: row.title.clone(),
            parent: row.parent,
            branch_role: row.branch_role.clone(),
            branch_row: row.branch_row,
        });
    }

    // Down, breadth-first, so a grandchild is drawn under its own parent and never under the root.
    let mut queue: Vec<(i64, usize)> = vec![(session, lineage_depth(&out, session))];
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    seen.insert(session);
    let mut guard = 0usize;
    while let Some((id, depth)) = queue.pop() {
        guard += 1;
        if guard > limit {
            dropped = dropped.saturating_add(1);
            continue;
        }
        for kid in store.children_of(id).map_err(|e| e.to_string())? {
            if !seen.insert(kid.id) {
                continue;
            }
            if out.len() >= limit {
                dropped += 1;
                continue;
            }
            let kids = store.children_of(kid.id).map_err(|e| e.to_string())?;
            out.push(ConversationNode {
                rows: store.branch_chain(kid.id).map(|c| c.len()).unwrap_or(0),
                off_branch: store.off_branch_rows(kid.id).unwrap_or(0),
                children: kids.len(),
                depth: depth + 1,
                current: false,
                parent_gone: false,
                branch_quote: quote_of_cut(store, &kid)?,
                session: kid.id,
                title: kid.title.clone(),
                parent: kid.parent,
                branch_role: kid.branch_role.clone(),
                branch_row: kid.branch_row,
            });
            queue.push((kid.id, depth + 1));
        }
    }
    Ok((out, dropped))
}

fn lineage_depth(nodes: &[ConversationNode], session: i64) -> usize {
    nodes.iter().find(|n| n.session == session).map(|n| n.depth).unwrap_or(0)
}

/// What the cut was made at, in the parent's words.
///
/// Three answers and they are different sentences: the row is there (quote it), the row is gone
/// (say that — the branch stands, but the person cannot read the cut), and there is no recorded
/// cut at all (a branch from before lineage existed). Guessing a row to quote would be the worst
/// of the three: it would put words on a cut nobody made.
fn quote_of_cut(store: &dyn StoreOps, row: &Session) -> Result<String, String> {
    let Some(parent) = row.parent else {
        return Ok("root — this conversation was not cut out of another".to_string());
    };
    if row.branch_role.is_empty() && row.branch_row.is_none() {
        return Ok(format!(
            "branched from conversation {parent} before the cut was recorded, so this tree cannot \
             name the message it starts from"
        ));
    }
    let Some(at) = row.branch_row else {
        return Ok(format!(
            "cut in conversation {parent} at a row this record does not name"
        ));
    };
    let rows = store.messages(parent).map_err(|e| e.to_string())?;
    match rows.iter().find(|m| m.id == at) {
        Some(m) => Ok(format!(
            "{} — {}",
            if m.role == "user" { "your message" } else { m.role.as_str() },
            quote(&m.content)
        )),
        None => Ok(format!(
            "conversation {parent} no longer holds that row (id {at}), so the cut cannot be quoted; \
             this branch keeps its own copy of what it carries"
        )),
    }
}

/// The node list of one conversation: its own rows, and — when pi answered — pi's entries matched
/// onto them.
/// The node list of one conversation: its rows, pi's entries when there are any, and what the two
/// records do not answer to.
///
/// Public because both cores and both transports draw from it, and a bridge test has to be able to
/// hand it a tree fixture whose file order is not its tree order — the one shape where reading pi's
/// JSONL as a list would draw a conversation that never happened.
///
/// Returns the nodes, the drift lines (capped at [`DRIFT_LINES`]), the number of drift lines there
/// are in total, and the number of nodes the card did not list.
pub fn nodes_of(
    store: &dyn StoreOps,
    session: i64,
    pi: Option<&PiTree>,
) -> (Vec<TreeNode>, Vec<String>, usize, usize) {
    let all = store.all_rows(session).unwrap_or_default();
    let chain_ids: Vec<i64> = store.branch_chain(session).unwrap_or_default();
    let chain: std::collections::HashSet<i64> = chain_ids.iter().copied().collect();
    let leaf = store.leaf_row(session).unwrap_or(None);
    // With no leaf written, the conversation reads its newest row, and that row is what the next
    // answer reads through. The card has to mark something, or a question about which node is
    // current has no answer on every conversation that was never switched. A leaf pointed at row 0,
    // the leaf in front of the first message, matches no node, which is the honest answer there.
    let tip = leaf.unwrap_or_else(|| chain_ids.last().copied().unwrap_or(0));

    // Rows by words, in file order, for the matching below. Two rows with the same words — "ok"
    // twice — are answered in the order they were written, which is the only order that can be
    // right when the words are identical.
    let mut by_words: std::collections::HashMap<String, std::collections::VecDeque<(i64, String)>> =
        std::collections::HashMap::new();
    for (id, _parent, role, content) in all.iter() {
        if !matches!(role.as_str(), "user" | "assistant" | "tool") {
            continue;
        }
        by_words
            .entry(norm(content))
            .or_default()
            .push_back((*id, role.clone()));
    }

    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut drift: Vec<String> = Vec::new();
    let mut drift_count = 0usize;
    // Depth by parent link, computed on the fly from the file order (`all` is ascending, and a
    // parent is always written before its child).
    let mut depth_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (id, parent, role, content) in all.iter() {
        let d = match parent {
            Some(p) => depth_of.get(p).map(|v| v + 1).unwrap_or(0),
            None => 0,
        };
        depth_of.insert(*id, d);
        nodes.push(TreeNode {
            row: Some(*id),
            pi_entry: None,
            role: role.clone(),
            quote: quote(content),
            content: content.clone(),
            on_branch: chain.contains(id),
            current: *id == tip,
            pi_current: false,
            depth: d,
            drift: false,
        });
    }

    if let Some(t) = pi {
        // Walk pi's tree in file order, and match each of its message entries to one row of the
        // same words. Anything left over on either side is a line of the tree, not a hole.
        let mut pi_depth: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut entry_of_row: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        let mut row_of_entry: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut rows_by_id: std::collections::HashMap<i64, &(i64, Option<i64>, String, String)> = std::collections::HashMap::new();
        for r in all.iter() {
            rows_by_id.insert(r.0, r);
        }
        for n in t.nodes.iter() {
            let d = match &n.parent {
                Some(p) => pi_depth.get(p).map(|v| v + 1).unwrap_or(0),
                None => 0,
            };
            pi_depth.insert(n.entry.clone(), d);
            if !pi_entry_is_mine(&n.kind, &n.role) {
                // pi's own writing: a node of the thread, never a disagreement about it.
                nodes.push(TreeNode {
                    row: None,
                    pi_entry: Some(n.entry.clone()),
                    role: n.role.clone(),
                    quote: quote(&n.text),
                    content: n.text.clone(),
                    on_branch: n.on_branch,
                    current: false,
                    pi_current: Some(n.entry.as_str()) == t.leaf.as_deref(),
                    depth: d,
                    drift: false,
                });
                continue;
            }
            let hit = by_words.get_mut(&norm(&n.text)).and_then(|q| q.pop_front());
            match hit {
                Some((rid, _)) => {
                    entry_of_row.insert(rid, n.entry.clone());
                    row_of_entry.insert(n.entry.clone(), rid);
                }
                None => {
                    // A node the record has nothing for. Off-path pi entries are counted too, but
                    // said as off-path, because they are not a disagreement about what is being
                    // answered — they are the other branch, which the record does hold.
                    drift_count += 1;
                    if drift.len() < DRIFT_LINES {
                        drift.push(drift_line_pi(n));
                    }
                    nodes.push(TreeNode {
                        row: None,
                        pi_entry: Some(n.entry.clone()),
                        role: n.role.clone(),
                        quote: quote(&n.text),
                        content: n.text.clone(),
                        on_branch: n.on_branch,
                        current: false,
                        pi_current: Some(n.entry.as_str()) == t.leaf.as_deref(),
                        depth: d,
                        drift: true,
                    });
                }
            }
        }
        // The rows pi has no entry for, quoted from the record itself.
        for (id, _parent, role, content) in all.iter() {
            if entry_of_row.contains_key(id) {
                continue;
            }
            // Off-branch rows are not part of what the model reads, so a pi thread that skips them
            // is not disagreeing about the answer being read.
            let off = !chain.contains(id);
            if !off {
                drift_count += 1;
                if drift.len() < DRIFT_LINES {
                    drift.push(drift_line_row(role, content));
                }
            }
            if let Some(node) = nodes.iter_mut().find(|n| n.row == Some(*id)) {
                node.drift = !off;
            }
        }
        // Which row each entry answers to, so the node list can carry both ids on one line.
        for node in nodes.iter_mut() {
            if let Some(rid) = node.row {
                node.pi_entry = entry_of_row.get(&rid).cloned();
            }
        }
        // pi's leaf, mapped onto the record: which row the model is *at*, in cwoffee's terms. This is
        // pi's mark, not the record's: `current` belongs to the record's leaf, and a card that moved
        // that mark onto pi's node would tell the person their next send joins pi's path when the
        // record says otherwise. A pi leaf nothing answers to marks nothing, which is the honest
        // reading of a drifted file.
        if let Some(l) = t.leaf.as_deref() {
            match row_of_entry.get(l) {
                Some(rid) => {
                    for node in nodes.iter_mut() {
                        if node.row == Some(*rid) {
                            node.pi_current = true;
                        }
                    }
                }
                None => {
                    if let Some(node) = nodes.iter_mut().find(|n| n.pi_entry.as_deref() == Some(l)) {
                        node.pi_current = true;
                        // Drift only when pi's leaf is a message the record has no row for. A leaf on
                        // pi's own bookkeeping is pi's writing, and the record was never meant to hold
                        // it — the same fact the entry above is drawn by, not a disagreement.
                        node.drift = t
                            .nodes
                            .iter()
                            .find(|n| n.entry.as_str() == l)
                            .map(|n| pi_entry_is_mine(&n.kind, &n.role))
                            .unwrap_or(false);
                    }
                }
            }
        }
        // Where each record says the model is. The record's leaf and pi's leaf are two answers, and
        // when they differ a card with two "read through" marks and no explanation is worse than one
        // that says the two records are at different nodes — which is the drift that makes the next
        // answer come from a history the person is not looking at.
        //
        // What is compared is the **message** each side is reading. pi's thread can end on its own
        // bookkeeping — the entry that recorded the model, a thinking level, a fold — and a thread
        // that holds only those is reading no message of the conversation, which is exactly what the
        // record says when its leaf sits in front of the first row. Calling that a disagreement would
        // cry wolf on a fork taken at a conversation's opening message.
        let mut pi_msg: Option<&PiNode> = None;
        if let Some(l) = t.leaf.as_deref() {
            let mut cur = Some(l);
            let mut guard = 0usize;
            while let Some(id) = cur {
                guard += 1;
                if guard > t.nodes.len() + 1 {
                    break;
                }
                let node = match t.nodes.iter().find(|n| n.entry == id) {
                    Some(n) => n,
                    None => break,
                };
                if pi_entry_is_mine(&node.kind, &node.role) {
                    pi_msg = Some(node);
                    break;
                }
                cur = node.parent.as_deref();
            }
        }
        let pi_at: Option<i64> = pi_msg.and_then(|n| row_of_entry.get(&n.entry).copied());
        let record_reads_something = tip != 0 && chain_ids.contains(&tip);
        let disagree = match pi_msg {
            // Neither side is reading a message: the record in front of its first row, pi on its own
            // bookkeeping. That is agreement about an empty conversation, not two records apart.
            None => record_reads_something,
            Some(_) => pi_at != Some(tip),
        };
        if disagree {
            drift_count += 1;
            if drift.len() < DRIFT_LINES {
                let theirs = pi_msg
                    .map(|n| quote(&n.text))
                    .unwrap_or_else(|| "no message of this conversation".to_string());
                let ours = rows_by_id
                    .get(&tip)
                    .map(|(_, _, _, c)| quote(c))
                    .unwrap_or_else(|| "nothing".to_string());
                drift.push(format!(
                    "pi's thread reads through {} while this conversation's leaf is at {} — the two records are at different nodes, so the next answer comes from a history the card is not showing",
                    theirs, ours
                ));
            }
        }
    }

    // Reading order: the branch the next answer reads first, then the rows and entries that are not
    // on it, each keeping their own depth so the tree still shows where they hang.
    nodes.sort_by_key(|n| (!n.on_branch, n.depth, n.row.unwrap_or(i64::MAX)));
    // The cap cuts off-path nodes only, because the sort put the branch first: the path the next
    // answer reads is never the part a card leaves out.
    let hidden = nodes.len().saturating_sub(TREE_NODES);
    if hidden > 0 {
        nodes.truncate(TREE_NODES);
    }
    (nodes, drift, drift_count, hidden)
}

/// Read the tree of one conversation: its lineage, its nodes, and what the two records disagree
/// about.
pub async fn read(store: &dyn StoreOps, host: &dyn HostOps, session: i64) -> Result<Tree, String> {
    let core = host.core_id().to_string();
    let (lineage, dropped) = lineage_of(store, session, 40)?;
    let pi = host.pi_tree(session).await;
    let pi_read = match &pi {
        Some(t) => PiRead {
            consulted: true,
            entries: t.nodes.len(),
            leaf_entry: t.leaf.clone(),
            file: t.file.clone(),
            why: String::new(),
        },
        None => PiRead {
            consulted: false,
            entries: 0,
            leaf_entry: None,
            file: String::new(),
            why: if core == "pi" {
                "the model process is not holding this conversation, so its tree could not be read".to_string()
            } else {
                "this engine keeps its memory in the record, so the record's own node list is the tree".to_string()
            },
        },
    };
    let (nodes, drift, drift_count, hidden) = nodes_of(store, session, pi.as_ref());
    let off_branch = store.off_branch_rows(session).map_err(|e| e.to_string())?;
    let leaf = store.leaf_row(session).map_err(|e| e.to_string())?;
    let note = tree_note(&core, &lineage, &pi_read, off_branch, dropped, drift_count, hidden);
    Ok(Tree {
        session,
        core,
        lineage,
        nodes,
        nodes_hidden: hidden,
        off_branch,
        leaf,
        pi: pi_read,
        drift,
        drift_count,
        note,
    })
}

/// One sentence about what was read, in the terms of the core that answered.
fn tree_note(
    core: &str,
    lineage: &[ConversationNode],
    pi: &PiRead,
    off_branch: usize,
    dropped: usize,
    drift_count: usize,
    hidden: usize,
) -> String {
    let kids = lineage.iter().filter(|n| n.current).map(|n| n.children).next().unwrap_or(0);
    let cut = match lineage.iter().find(|n| n.current) {
        Some(n) if n.parent.is_some() => format!(", cut from conversation {}", n.parent.unwrap_or(0)),
        Some(_) => ", a root — nothing was cut out of it".to_string(),
        None => String::new(),
    };
    let memory = if core == "pi" {
        if pi.consulted {
            format!(
                " pi's thread was read: {} entries, its leaf is {}",
                pi.entries,
                pi.leaf_entry.clone().unwrap_or_else(|| "none".to_string())
            )
        } else {
            format!(" pi's tree was not read — {}", pi.why)
        }
    } else {
        " this engine answers from the record's own node list".to_string()
    };
    let left = if off_branch > 1 {
        format!(", {off_branch} rows on branches that are not being read")
    } else if off_branch == 1 {
        ", 1 row on a branch that is not being read".to_string()
    } else {
        String::new()
    };
    let disagree = if drift_count > 0 {
        format!(", and {drift_count} node(s) the two records do not answer to each other")
    } else {
        ", and the record and the model's memory answer to each other".to_string()
    };
    let more = if dropped > 0 {
        format!(", {dropped} branch(es) not listed — the tree is capped at 40 conversations")
    } else {
        String::new()
    };
    // The node list has a cap too, and what it drops is always off-path. Naming the number keeps a
    // short card from reading like a short conversation.
    let few = if hidden > 0 {
        format!(
            ", {} node{} not listed on this card",
            hidden,
            if hidden == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    format!("{kids} branch(es) hang off this conversation{cut}{memory}{left}{disagree}{more}{few}.")
}

/// What was asked: which node to move the leaf to.
///
/// The identity discipline is [`crate::branch::BranchReq`]'s and for the same reason: with a
/// desktop window, a phone tab and a harness all able to reach one conversation, "the third
/// message" can land on a row this window never showed. So the caller names the row and repeats
/// the words it painted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LeafReq {
    pub session: i64,
    #[serde(default)]
    pub message_id: Option<i64>,
    /// Count back from the newest message whose role is `role`. 1 is the newest.
    #[serde(default)]
    pub from_end: Option<usize>,
    #[serde(default)]
    pub role: String,
    /// The words this window is showing on that row.
    #[serde(default)]
    pub shown: String,
}

/// Which node was asked for, and where the leaf goes because of it.
#[derive(Debug, Clone)]
pub struct LeafTarget {
    pub row_id: i64,
    pub role: String,
    /// Where the leaf lands. For one of the person's own messages that is its **parent** — the
    /// message itself leaves what the next answer reads, and comes back to be said again, which is
    /// what pi's own navigation does with it. For an answer it is the answer.
    pub leaf: Option<i64>,
    /// Words to hand back to the composer, for the same reason a branch does: the message that
    /// leaves the context is the one the person is about to say again.
    pub prefill: Option<String>,
}

/// Resolve the node a client named.
///
/// Looked up in **every** row of the conversation, not only the branch being read: a node left on an
/// abandoned path is the one a person most wants to go back to, and the tree card lists it. The
/// identity guard is [`crate::branch::resolve`]'s and for the same reason — with a desktop window, a
/// phone tab and a harness all able to reach one conversation, "the third message" can land on a row
/// this window never showed.
pub fn resolve(all: &[(i64, Option<i64>, String, String)], req: &LeafReq) -> Result<LeafTarget, String> {
    if all.is_empty() {
        return Err(format!(
            "conversation {} holds nothing, so there is no node to move the leaf to",
            req.session
        ));
    }
    let role = match req.role.as_str() {
        "assistant" => "assistant",
        _ => "user",
    };
    let found: &(i64, Option<i64>, String, String) = if let Some(id) = req.message_id {
        all.iter().find(|(mid, _, _, _)| *mid == id).ok_or_else(|| {
            format!(
                "conversation {} holds no message {id}, so the leaf was not moved. Reload the \
                 conversation and pick a node you can see.",
                req.session
            )
        })?
    } else if let Some(from_end) = req.from_end {
        let same: Vec<&(i64, Option<i64>, String, String)> = all.iter().filter(|(_, _, r, _)| r == role).collect();
        if from_end == 0 || from_end > same.len() {
            return Err(format!(
                "this window asked for the {from_end}th {} counting back from the newest, and \
                 conversation {} has {}. The leaf was not moved — what is on screen is not what the \
                 record holds; reload the conversation.",
                role,
                req.session,
                same.len()
            ));
        }
        same[same.len() - from_end]
    } else {
        return Err(format!(
            "moving the leaf has to name the node it moves to: a stored row id, or a position \
             counted back from the newest {} of conversation {}. Nothing was moved.",
            if role == "assistant" { "answer" } else { "message" },
            req.session
        ));
    };
    if req.shown != found.3 {
        return Err(format!(
            "that message says something else now — {} — so the leaf was not moved. Reload the \
             conversation and move the leaf to a node you can see.",
            quote(&found.3)
        ));
    }
    // The leaf position follows the node's role, which is pi's rule and now this engine's too:
    // your own message leaves what the next answer reads, an answer stays in it.
    let leaf = if found.2 == "user" {
        // A user row with no parent is the conversation's opening ask: the leaf goes *before* it,
        // which is the node pi's navigation leaves you at — nothing read, every row off the path.
        found.1.or(Some(0))
    } else {
        Some(found.0)
    };
    Ok(LeafTarget {
        row_id: found.0,
        role: found.2.clone(),
        leaf,
        prefill: (found.2 == "user").then(|| found.3.clone()),
    })
}

/// What the switch did, in the shape the client and the receipt read.
#[derive(Debug, Clone, Serialize)]pub struct LeafOutcome {
    pub session: i64,
    /// Where the leaf is now. `None` means the newest row.
    pub leaf: Option<i64>,
    /// The node the next answer reads through, quoted.
    pub reads_through: String,
    /// Rows now off the path the next answer reads.
    pub off_branch: usize,
    /// Words to put back in the composer, when the switch took one of your own messages out of
    /// what the next answer reads.
    pub prefill: Option<String>,
    pub note: String,
}

pub fn outcome_note(store: &dyn StoreOps, session: i64, target: &LeafTarget, memory: &str) -> Result<String, String> {
    let rows = store.messages(session).map_err(|e| e.to_string())?;
    let leaf = store.leaf_row(session).map_err(|e| e.to_string())?;
    let off = store.off_branch_rows(session).map_err(|e| e.to_string())?;
    let reads = rows.len();
    let through = match leaf {
        Some(0) => "nothing yet — the model reads only its prompt".to_string(),
        Some(id) => rows
            .iter()
            .find(|m| m.id == id)
            .map(|m| format!("row {} — {}", m.id, quote(&m.content)))
            .unwrap_or_else(|| format!("row {id}")),
        None => "its newest message".to_string(),
    };
    let ask = if target.prefill.is_some() {
        " Your words are back in the composer — say them again and that branch answers them."
    } else {
        ""
    };
    let off_line = if off > 1 {
        format!(" {off} later rows stay in the record and are not read; nothing was deleted.")
    } else if off == 1 {
        " 1 later row stays in the record and is not read; nothing was deleted.".to_string()
    } else {
        " Nothing was left behind: this is the newest path.".to_string()
    };
    Ok(format!(
        "the leaf moved to {through}: the next answer reads {reads} message(s){ask}.{off_line} {memory}"
    ))
}

/// The whole ask, end to end: refuse mid-turn, check which node was meant, and let the host move
/// whichever memory it owns.
///
/// One entry point for both transports, because the busy rule and the proof of which node was meant
/// are the substance of the verb — a phone must not be able to move a leaf the desktop refuses.
pub async fn move_leaf(store: &dyn StoreOps, host: &dyn HostOps, req: &LeafReq) -> Result<LeafOutcome, String> {
    let st = host.state();
    if st.busy {
        return Err(format!(
            "a turn is running on conversation {}, so the leaf was not moved: stop it, or wait for \
             it to finish, and then move the leaf. The next answer would otherwise read a path that \
             changed under it.",
            st.session_id
        ));
    }
    if store.session(req.session).map_err(|e| e.to_string())?.is_none() {
        return Err(format!(
            "conversation {} does not exist, so the leaf was not moved",
            req.session
        ));
    }
    let all = store.all_rows(req.session).map_err(|e| e.to_string())?;
    let target = resolve(&all, req)?;

    // The host moves its own memory and, with it, the record's leaf — one write, one owner, so a
    // refusal never leaves a record pointing somewhere the model is not. Re-reading the leaf below is
    // not a guess: it is what the host says it left it at.
    let moved = host.move_leaf(req.session, &target).await?;
    let rows = store.messages(req.session).map_err(|e| e.to_string())?;
    let note = outcome_note(store, req.session, &target, &moved)?;
    Ok(LeafOutcome {
        session: req.session,
        leaf: store.leaf_row(req.session).map_err(|e| e.to_string())?,
        reads_through: if target.role == "user" {
            format!("before your message {}", quote(&rows.iter().find(|m| m.id == target.row_id).map(|m| m.content.clone()).unwrap_or_else(|| all.iter().find(|(id, _, _, _)| *id == target.row_id).map(|r| r.3.clone()).unwrap_or_default())))
        } else {
            format!("through the answer {}", quote(&rows.iter().find(|m| m.id == target.row_id).map(|m| m.content.clone()).unwrap_or_else(|| all.iter().find(|(id, _, _, _)| *id == target.row_id).map(|r| r.3.clone()).unwrap_or_default())))
        },
        off_branch: store.off_branch_rows(req.session).map_err(|e| e.to_string())?,
        prefill: target.prefill.clone(),
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use std::sync::Arc;
    use crate::store::Store;

    fn seeded() -> (Store, i64, Vec<i64>) {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Roast notes", "/tmp", "m").unwrap();
        let mut ids = Vec::new();
        for (role, text) in [
            ("user", "first ask"),
            ("assistant", "first answer"),
            ("user", "second ask"),
            ("assistant", "second answer"),
        ] {
            let m = match role {
                "user" => Message::user(text),
                _ => Message::assistant(text),
            };
            ids.push(s.append_message(c, &m).unwrap());
        }
        (s, c, ids)
    }

    /// A root says it is a root. `None` is a fact about the conversation, not a hole in the data.
    #[test]
    fn a_conversation_with_no_lineage_is_a_root_and_says_so() {
        let (s, c, ids) = seeded();
        let kids = s.children_of(c).unwrap();
        assert!(kids.is_empty(), "nothing was cut out of it");
        let q = quote_of_cut(&s, &s.session(c).unwrap().unwrap()).unwrap();
        assert!(q.starts_with("root"), "{q}");
        let row = s.session(c).unwrap().unwrap();
        assert!(row.parent.is_none() && row.branch_row.is_none());
        assert_eq!(row.leaf_row, Some(ids[3]), "a conversation nobody switched reads its newest row");
    }

    /// Lineage is written by the branch and read back as parent + cut.
    #[test]
    fn a_branch_records_its_parent_and_the_row_it_was_cut_at() {
        let (s, c, ids) = seeded();
        let child = s.create_session("Roast notes — branch of Roast notes", "/tmp", "m").unwrap();
        let copied = s.copy_through(c, child, ids[1]).unwrap();
        assert_eq!(copied, 2);
        s.set_lineage(child, c, "assistant", Some(ids[1])).unwrap();
        let row = s.session(child).unwrap().unwrap();
        assert_eq!(row.parent, Some(c));
        assert_eq!(row.branch_role, "assistant");
        assert_eq!(row.branch_row, Some(ids[1]));
        let kids = s.children_of(c).unwrap();
        assert_eq!(kids.len(), 1, "the parent knows its child");
        assert_eq!(kids[0].id, child);
        assert_eq!(row.leaf_row, Some(ids_copied(&s, child, 2)), "the copy is one branch");
    }

    fn ids_copied(s: &Store, session: i64, n: usize) -> i64 {
        s.branch_chain(session).unwrap()[n - 1]
    }

    /// A cut whose row is gone is quoted as gone; the branch is not dropped and no words are made up.
    #[test]
    fn a_cut_whose_row_is_gone_is_said_rather_than_quoted() {
        let (s, c, ids) = seeded();
        let child = s.create_session("branch", "/tmp", "m").unwrap();
        s.copy_through(c, child, ids[1]).unwrap();
        s.set_lineage(child, c, "assistant", Some(ids[3])).unwrap();
        // The row the cut names is not in the parent any more.
        s.truncate_from(c, ids[3]).unwrap();
        let q = quote_of_cut(&s, &s.session(child).unwrap().unwrap()).unwrap();
        assert!(q.contains("no longer holds that row"), "{q}");
        assert!(q.contains(&ids[3].to_string()), "the id it cannot find is named: {q}");
        assert_eq!(s.session(child).unwrap().unwrap().parent, Some(c), "the child keeps its lineage");
    }

    /// A branch made before lineage existed says it cannot name its cut, instead of naming one.
    #[test]
    fn a_branch_with_no_recorded_cut_names_nothing() {
        let (s, c, _) = seeded();
        let child = s.create_session("old branch", "/tmp", "m").unwrap();
        s.set_lineage(child, c, "", None).unwrap();
        let q = quote_of_cut(&s, &s.session(child).unwrap().unwrap()).unwrap();
        assert!(q.contains("before the cut was recorded"), "{q}");
    }

    /// The leaf is the newest row until somebody moves it, and moving it reads a prefix of the
    /// branch instead of the whole conversation.
    #[test]
    fn moving_the_leaf_changes_what_is_read_and_deletes_nothing() {
        let (s, c, ids) = seeded();
        assert_eq!(s.leaf_row(c).unwrap(), Some(ids[3]), "the newest row is the leaf");
        assert_eq!(s.messages(c).unwrap().len(), 4);
        assert_eq!(s.history(c).unwrap().len(), 4);

        s.set_leaf(c, Some(ids[1])).unwrap();
        let rows = s.messages(c).unwrap();
        assert_eq!(rows.len(), 2, "the next answer reads up to that answer");
        assert_eq!(rows[1].id, ids[1]);
        assert_eq!(s.off_branch_rows(c).unwrap(), 2, "and says what it left out");
        assert_eq!(s.message_count(c).unwrap(), 4, "nothing was deleted");
        // Paging reads the same branch, newest window first.
        let page = s.messages_page(c, None, 1).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, ids[1]);
        assert!(!page.has_older || page.messages.len() == 1, "the window holds the newest row of the branch");
    }

    /// A message sent after a leaf switch joins *that* branch, not the newest row in the file.
    #[test]
    fn a_message_sent_after_a_leaf_switch_joins_the_branch_it_was_left_on() {
        let (s, c, ids) = seeded();
        s.set_leaf(c, Some(ids[1])).unwrap();
        let fresh = s.append_message(c, &Message::user("asked from the branch")).unwrap();
        let rows = s.messages(c).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].id, fresh);
        assert_eq!(rows[2].content, "asked from the branch");
        assert_eq!(s.all_rows(c).unwrap().len(), 5, "the abandoned path is still in the record");
        assert_eq!(s.off_branch_rows(c).unwrap(), 2);
        assert_eq!(s.leaf_row(c).unwrap(), Some(fresh), "the leaf advanced with the send");
    }

    /// Two leaves under one parent: a conversation whose rows are a tree, not a line.
    #[test]
    fn one_conversation_can_hold_two_branches_of_its_own() {
        let (s, c, ids) = seeded();
        s.set_leaf(c, Some(ids[1])).unwrap();
        let a = s.append_message(c, &Message::assistant("answer on path A")).unwrap();
        s.set_leaf(c, Some(ids[1])).unwrap();
        let b = s.append_message(c, &Message::assistant("answer on path B")).unwrap();
        let rows = s.messages(c).unwrap();
        assert_eq!(rows.len(), 3, "the read is the branch the leaf sits on");
        assert_eq!(rows[2].id, b, "path B is the one being read");
        assert_eq!(
            s.off_branch_rows(c).unwrap(),
            3,
            "path A's answer and the two later messages stay in the record"
        );
        let all = s.all_rows(c).unwrap();
        let (_id, parent, role, _) = all.iter().find(|(id, _, _, _)| *id == a).unwrap();
        assert_eq!(*parent, Some(ids[1]), "both answers hang off the same row");
        assert_eq!(*role, "assistant");
        assert!(all.iter().any(|(id, _, _, _)| *id == b), "path B's answer is in the file too");
    }

    /// A rewind that deletes the leaf puts it back on the newest surviving row.
    #[test]
    fn a_rewind_that_deletes_the_leaf_moves_it_back_and_leaves_earlier_leaves_alone() {
        let (s, c, ids) = seeded();
        s.set_leaf(c, Some(ids[1])).unwrap();
        let later = s.append_message(c, &Message::user("from the branch")).unwrap();
        s.truncate_from(c, later).unwrap();
        assert_eq!(s.leaf_row(c).unwrap(), Some(ids[1]), "the leaf was inside the cut");
        assert_eq!(s.messages(c).unwrap().len(), 2);

        // A leaf earlier than the cut is not moved: it marks a path the cut did not reach.
        s.set_leaf(c, Some(ids[0])).unwrap();
        s.truncate_from(c, ids[2]).unwrap();
        assert_eq!(s.leaf_row(c).unwrap(), Some(ids[0]), "the older leaf stands");
    }

    /// A leaf cannot be pointed at another conversation's row.
    #[test]
    fn a_leaf_that_points_at_another_conversation_is_refused() {
        let s = Store::in_memory().unwrap();
        let a = s.create_session("One", "/tmp", "m").unwrap();
        let b = s.create_session("Two", "/tmp", "m").unwrap();
        for (sess, text) in [(a, "ask a"), (b, "ask b"), (a, "answer a"), (b, "answer b")] {
            s.append_message(sess, &Message::user(text)).unwrap();
        }
        let b_ids = s.messages(b).unwrap().into_iter().map(|m| m.id).collect::<Vec<_>>();
        let a_tip = s.leaf_row(a).unwrap();
        let e = s.set_leaf(a, Some(b_ids[1])).unwrap_err();
        assert!(e.to_string().contains("belongs to conversation"), "{e}");
        assert_eq!(s.leaf_row(a).unwrap(), a_tip, "the leaf stayed where it was");
        // A row that is gone is refused too, in different words.
        let e = s.set_leaf(a, Some(9999)).unwrap_err();
        assert!(e.to_string().contains("not in the record any more"), "{e}");
    }

    /// Resolving the node asked for, with the same guard a branch uses.
    #[test]
    fn the_node_being_moved_has_to_be_proved_not_guessed() {
        let (s, c, ids) = seeded();
        let all = s.all_rows(c).unwrap();
        // At an answer: the leaf goes to that answer.
        let t = resolve(
            &all,
            &LeafReq { session: c, message_id: Some(ids[1]), role: "assistant".into(), shown: "first answer".into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(t.leaf, Some(ids[1]));
        assert!(t.prefill.is_none(), "an answer is not something to ask again");
        // At one of your messages: the leaf goes to its parent and the words come back.
        let u = resolve(
            &all,
            &LeafReq { session: c, message_id: Some(ids[2]), role: "user".into(), shown: "second ask".into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(u.leaf, Some(ids[1]), "the message itself leaves what the next answer reads");
        assert_eq!(u.prefill.as_deref(), Some("second ask"));
        // A row whose words moved.
        s.edit_message(ids[2], "someone else's ask").unwrap();
        let e = resolve(
            &s.all_rows(c).unwrap(),
            &LeafReq { session: c, message_id: Some(ids[2]), role: "user".into(), shown: "second ask".into(), ..Default::default() },
        )
        .unwrap_err();
        assert!(e.contains("says something else now"), "{e}");
        // Nothing named.
        let e = resolve(&all, &LeafReq { session: c, shown: "first ask".into(), ..Default::default() }).unwrap_err();
        assert!(e.contains("name the node"), "{e}");
        // A position this window invented.
        let e = resolve(
            &all,
            &LeafReq { session: c, from_end: Some(9), role: "user".into(), shown: "first ask".into(), ..Default::default() },
        )
        .unwrap_err();
        assert!(e.contains("has 2"), "the true count belongs in the refusal: {e}");
    }

    /// A row written before lineage existed reads as a line — the migration must not collapse an
    /// old database to its newest row.
    #[test]
    fn rows_with_no_recorded_parent_read_in_the_order_they_were_written() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Old store", "/tmp", "m").unwrap();
        for i in 0..3 {
            s.append_message(c, &Message::user(format!("m{i}"))).unwrap();
        }
        // Drop the lineage the new writes recorded, to look like a v15 database.
        {
            let conn = s.raw();
            conn.execute("UPDATE messages SET parent_row = NULL", []).unwrap();
            conn.execute("UPDATE sessions SET leaf_row = NULL", []).unwrap();
        }
        let rows = s.messages(c).unwrap();
        assert_eq!(rows.len(), 3, "the whole conversation is still the conversation");
        assert_eq!(rows[2].content, "m2");
        assert_eq!(s.leaf_row(c).unwrap(), None, "no leaf means the newest row");
    }

    /// An old database gains the columns without losing what it held.
    #[test]
    fn a_v15_database_gains_lineage_and_reads_the_same() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Carried over", "/tmp", "m").unwrap();
        for (role, text) in [("user", "ask"), ("assistant", "answer")] {
            let m = match role {
                "user" => Message::user(text),
                _ => Message::assistant(text),
            };
            s.append_message(c, &m).unwrap();
        }
        {
            let conn = s.raw();
            conn.execute("UPDATE messages SET parent_row = NULL", []).unwrap();
            conn.execute("UPDATE sessions SET parent = NULL, branch_role = '', branch_row = NULL, leaf_row = NULL", []).unwrap();
            conn.pragma_update(None, "user_version", 15).unwrap();
        }
        drop(s);
        let path = format!("/tmp/cwoffee-tree-migrate-{}.db", std::process::id());
        let _ = std::fs::remove_file(&path);
        let fresh = Store::open(&path).unwrap();
        {
            let conn = fresh.raw();
            conn.execute_batch(
                "INSERT INTO sessions (id, title, workspace, model, created_at, updated_at, persona, web_on) VALUES (7,'x','/tmp','m',1,1,'cowork',0);
                 INSERT INTO messages (id, session_id, role, content, created_at) VALUES (1,7,'user','ask',1),(2,7,'assistant','answer',1);",
            )
            .unwrap();
        }
        let rows = fresh.messages(7).unwrap();
        assert_eq!(rows.len(), 2, "a database that never had lineage still reads whole");
        assert_eq!(fresh.leaf_row(7).unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    /// The node list marks the current node and says which rows are off the path.
    #[test]
    fn the_node_list_marks_the_leaf_and_the_rows_it_is_not_reading() {
        let (s, c, ids) = seeded();
        s.set_leaf(c, Some(ids[1])).unwrap();
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, None);
        assert_eq!(drift_count, 0, "one record cannot disagree with itself");
        assert!(drift.is_empty());
        assert_eq!(nodes.iter().filter(|n| n.row.is_some()).count(), 4);
        assert_eq!(nodes.iter().filter(|n| n.on_branch).count(), 2);
        let current = nodes.iter().find(|n| n.current).unwrap();
        assert_eq!(current.row, Some(ids[1]));
        assert!(nodes.iter().any(|n| !n.on_branch && n.row == Some(ids[3])));
    }

    /// Nobody moved the leaf on most conversations, and "which node is current" still has an answer:
    /// the newest row on the branch. A card that marked nothing there would ask a question the
    /// conversation cannot answer.
    #[test]
    fn a_conversation_nobody_moved_marks_its_newest_row_as_current() {
        let (s, c, ids) = seeded();
        // What a conversation written before the record kept a leaf looks like: rows, and no leaf.
        s.set_leaf(c, None).unwrap();
        assert_eq!(s.leaf_row(c).unwrap(), None, "no leaf written");
        let (nodes, _, _, _) = nodes_of(&s, c, None);
        let marked: Vec<&TreeNode> = nodes.iter().filter(|n| n.current).collect();
        assert_eq!(marked.len(), 1, "exactly one node is what the next answer reads through");
        assert_eq!(marked[0].row, Some(ids[3]));
    }

    /// Taking back the conversation's opening ask leaves the leaf in front of every row. Nothing is
    /// current then, and the card says so rather than pointing at the row it took back.
    #[test]
    fn a_leaf_in_front_of_the_first_message_marks_no_node_and_reads_nothing() {
        let (s, c, _ids) = seeded();
        s.set_leaf(c, Some(0)).unwrap();
        let (nodes, _, _, _) = nodes_of(&s, c, None);
        assert!(nodes.iter().all(|n| !n.current), "nothing is being read, so nothing is marked");
        assert_eq!(s.history(c).unwrap().len(), 0, "the model is handed nothing to read");
    }

    /// The card quotes a node and carries it whole, and the two are different jobs.
    ///
    /// `shown` is the proof that this window meant *this* row, so the text has to travel intact. A
    /// card that only painted a 70-character quote could not make that proof, and a move addressed by
    /// row id alone would move whatever that id happened to hold by now.
    #[test]
    fn a_node_is_quoted_short_on_the_card_and_carried_whole_to_the_move() {
        let (s, c, ids) = seeded();
        let long = "roast ".repeat(60);
        s.append_message(c, &Message::user(&long)).unwrap();
        let (nodes, _, _, _) = nodes_of(&s, c, None);
        let short = nodes.iter().find(|n| n.row == Some(ids[2])).unwrap();
        assert_eq!(short.content, "second ask");
        let big = nodes.iter().find(|n| n.content == long).unwrap();
        assert!(
            big.quote.chars().count() <= 80,
            "the card quotes a turn, it does not paste it: {:?}",
            big.quote
        );
        assert_eq!(
            big.content.chars().count(),
            long.chars().count(),
            "the proof travels whole — a truncated quote would be refused as a row nobody showed"
        );
    }

    /// A card that lists a conversation's whole node list is fine until the conversation is long, and
    /// then it is a wall. What gets dropped is always the part that is not being read, and the number
    /// dropped is said out loud, so a short card is not mistaken for a short conversation.
    #[test]
    fn a_long_conversations_card_keeps_its_branch_and_says_what_it_dropped() {
        let (s, c, ids) = seeded();
        s.set_leaf(c, Some(ids[0])).unwrap();
        for i in 0..200 {
            s.append_message(c, &Message::assistant(format!("side answer {i}"))).unwrap();
        }
        s.set_leaf(c, Some(ids[3])).unwrap();
        assert_eq!(s.off_branch_rows(c).unwrap(), 200);
        let (nodes, _, _, hidden) = nodes_of(&s, c, None);
        assert_eq!(nodes.len(), TREE_NODES, "the card has a ceiling");
        assert_eq!(hidden, 204 - TREE_NODES);
        assert_eq!(
            nodes.iter().filter(|n| n.on_branch).count(),
            4,
            "the branch the next answer reads is never the part that gets dropped"
        );
        assert!(nodes.iter().any(|n| n.current && n.row == Some(ids[3])));
        let n = tree_note("rust", &[ConversationNode {
            session: c, title: "Roast notes".into(), parent: None, branch_role: String::new(),
            branch_row: None, branch_quote: String::new(), current: true, parent_gone: false,
            rows: 4, off_branch: 200, depth: 0, children: 0,
        }], &PiRead::default(), 200, 0, 0, hidden);
        assert!(n.contains(&format!("{hidden} nodes not listed")), "the note said: {n}");
    }

    /// pi's bookkeeping entries are nodes of the thread and not disagreements: the record was never
    /// meant to hold the model the process picked.
    #[test]
    fn pis_own_entries_are_nodes_and_not_drift() {
        let (s, c, ids) = seeded();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "e1".into(), parent: None, kind: "model_change".into(), role: "model".into(), text: String::new(), on_branch: true },
                PiNode { entry: "e2".into(), parent: Some("e1".into()), kind: "message".into(), role: "user".into(), text: "first ask".into(), on_branch: true },
                PiNode { entry: "e3".into(), parent: Some("e2".into()), kind: "message".into(), role: "assistant".into(), text: "first answer".into(), on_branch: true },
                PiNode { entry: "e4".into(), parent: Some("e3".into()), kind: "message".into(), role: "user".into(), text: "second ask".into(), on_branch: true },
                PiNode { entry: "e5".into(), parent: Some("e4".into()), kind: "message".into(), role: "assistant".into(), text: "second answer".into(), on_branch: true },
            ],
            leaf: Some("e5".into()),
            file: "/tmp/s.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        assert_eq!(drift_count, 0, "{drift:?}");
        assert!(drift.is_empty());
        let pi_node = nodes.iter().find(|n| n.pi_entry.as_deref() == Some("e2")).unwrap();
        assert_eq!(pi_node.row, Some(ids[0]), "the entry and the row answer to each other");
        assert!(nodes.iter().any(|n| n.pi_entry.as_deref() == Some("e1") && n.row.is_none()));
        assert!(nodes.iter().any(|n| n.current && n.pi_entry.as_deref() == Some("e5")));
    }

    /// A thread with a message no row answers to reports itself — that is the drifted file, and the
    /// tree's whole reason for existing.
    #[test]
    fn a_thread_the_record_does_not_answer_to_reports_itself() {
        let (s, c, _) = seeded();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "p1".into(), parent: None, kind: "message".into(), role: "system".into(), text: "You are a helpful assistant.".into(), on_branch: true },
                PiNode { entry: "p2".into(), parent: Some("p1".into()), kind: "message".into(), role: "user".into(), text: "a message pi remembers and the record does not".into(), on_branch: true },
            ],
            leaf: Some("p2".into()),
            file: "/tmp/replaced.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        assert!(drift_count >= 1, "the pi side's extra message is counted");
        assert!(
            drift.iter().any(|d| d.contains("pi's thread holds") && d.contains("no row")),
            "{drift:?}"
        );
        assert!(nodes.iter().any(|n| n.pi_entry.as_deref() == Some("p2") && n.row.is_none()));
        // And rows pi has nothing for are drift in the other direction.
        assert!(drift.iter().any(|d| d.contains("pi's thread has no entry")), "{drift:?}");
    }

    /// Off-branch pi entries are named as off-branch, so a tree that shows a second path does not
    /// read as a disagreement about the path being answered.
    #[test]
    fn a_pi_path_that_is_not_being_read_says_so() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Two paths", "/tmp", "m").unwrap();
        let a = s.append_message(c, &Message::user("ask")).unwrap();
        s.append_message(c, &Message::assistant("answer A")).unwrap();
        s.set_leaf(c, Some(a)).unwrap();
        s.append_message(c, &Message::assistant("answer B")).unwrap();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "u".into(), parent: None, kind: "message".into(), role: "user".into(), text: "ask".into(), on_branch: true },
                PiNode { entry: "x".into(), parent: Some("u".into()), kind: "message".into(), role: "assistant".into(), text: "answer A".into(), on_branch: false },
                PiNode { entry: "y".into(), parent: Some("u".into()), kind: "message".into(), role: "assistant".into(), text: "answer B".into(), on_branch: true },
            ],
            leaf: Some("y".into()),
            file: "/tmp/two.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        assert_eq!(drift_count, 0, "two paths in both records is not drift: {drift:?}");
        assert!(nodes.iter().any(|n| n.pi_entry.as_deref() == Some("x") && !n.on_branch));
        assert!(nodes.iter().any(|n| n.current && n.pi_entry.as_deref() == Some("y")));
    }

    /// The record's leaf and pi's leaf are two answers, and each keeps its own mark.
    ///
    /// This is the state a pi-core card reaches when a leaf move half-applied, or when pi's thread was
    /// moved by something cwoffee didn't ask for. Marking pi's node as *the* leaf would tell the person
    /// their next send joins pi's path while the record says it joins the other one; marking only the
    /// record would hide that the model is somewhere else. Two marks and one sentence is the honest
    /// answer, and it is the only shape that survives `leaf = 0` — where the record reads nothing at
    /// all and pi still has a thread.
    #[test]
    fn a_record_and_a_thread_at_different_nodes_keep_their_own_marks_and_say_so() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Two answers", "/tmp", "m").unwrap();
        s.append_message(c, &Message::user("ask")).unwrap();
        let ans = s.append_message(c, &Message::assistant("answer A")).unwrap();
        s.append_message(c, &Message::user("second ask")).unwrap();
        s.append_message(c, &Message::assistant("answer B")).unwrap();
        // The record stands at the first answer; pi's thread is at the newest one.
        s.set_leaf(c, Some(ans)).unwrap();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "u".into(), parent: None, kind: "message".into(), role: "user".into(), text: "ask".into(), on_branch: true },
                PiNode { entry: "x".into(), parent: Some("u".into()), kind: "message".into(), role: "assistant".into(), text: "answer A".into(), on_branch: true },
                PiNode { entry: "y".into(), parent: Some("x".into()), kind: "message".into(), role: "user".into(), text: "second ask".into(), on_branch: true },
                PiNode { entry: "z".into(), parent: Some("y".into()), kind: "message".into(), role: "assistant".into(), text: "answer B".into(), on_branch: true },
            ],
            leaf: Some("z".into()),
            file: "/tmp/two.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        let ours = nodes.iter().find(|n| n.current).expect("the record marks its own leaf");
        assert_eq!(ours.row, Some(ans), "and that mark is the row the record reads through");
        assert!(!ours.pi_current, "pi is not standing on it");
        let theirs = nodes
            .iter()
            .find(|n| n.pi_current && n.pi_entry.as_deref() == Some("z"))
            .expect("pi's node carries pi's mark");
        assert!(!theirs.current, "and it does not borrow the record's mark");
        assert_eq!(drift_count, 1, "one disagreement, named: {drift:?}");
        assert!(
            drift.iter().any(|d| d.contains("different nodes") && d.contains("answer B") && d.contains("answer A")),
            "{drift:?}"
        );
    }

    /// pi's leaf with the record reading nothing: the record marks no node, pi still marks its own,
    /// and the sentence names both.
    #[test]
    fn a_record_reading_nothing_marks_no_node_even_when_pi_has_a_leaf() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Taken back", "/tmp", "m").unwrap();
        s.append_message(c, &Message::user("ask")).unwrap();
        s.append_message(c, &Message::assistant("answer")).unwrap();
        s.set_leaf(c, Some(0)).unwrap();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "u".into(), parent: None, kind: "message".into(), role: "user".into(), text: "ask".into(), on_branch: true },
                PiNode { entry: "a".into(), parent: Some("u".into()), kind: "message".into(), role: "assistant".into(), text: "answer".into(), on_branch: true },
            ],
            leaf: Some("a".into()),
            file: "/tmp/taken.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        assert!(nodes.iter().all(|n| !n.current), "the record reads nothing, so nothing is the leaf");
        assert!(nodes.iter().any(|n| n.pi_current && n.pi_entry.as_deref() == Some("a")));
        assert_eq!(drift_count, 1, "and the difference is said, not shown twice: {drift:?}");
        assert!(drift.iter().any(|d| d.contains("different nodes")), "{drift:?}");
    }

    /// A pi thread that ends on its own bookkeeping is reading no message of the conversation, and a
    /// record whose leaf sits in front of the first row is reading nothing: two records saying the
    /// same thing, not two records apart.
    ///
    /// This is the state a fork at a conversation's opening message leaves behind — pi's new thread
    /// holds the entry that recorded the model and nothing else. Marking it as a disagreement would put
    /// a warning on the card for a conversation whose next answer genuinely starts from nothing.
    #[test]
    fn a_thread_on_its_own_bookkeeping_and_a_record_reading_nothing_agree() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Forked at the top", "/tmp", "m").unwrap();
        s.append_message(c, &Message::user("ask")).unwrap();
        s.append_message(c, &Message::assistant("answer")).unwrap();
        s.set_leaf(c, Some(0)).unwrap();
        let t = PiTree {
            nodes: vec![
                PiNode { entry: "u".into(), parent: None, kind: "message".into(), role: "user".into(), text: "ask".into(), on_branch: false },
                PiNode { entry: "a".into(), parent: Some("u".into()), kind: "message".into(), role: "assistant".into(), text: "answer".into(), on_branch: false },
                PiNode { entry: "m".into(), parent: None, kind: "model_change".into(), role: "model".into(), text: "cwoffee-mock".into(), on_branch: true },
            ],
            leaf: Some("m".into()),
            file: "/tmp/fork.jsonl".into(),
        };
        let (nodes, drift, drift_count, _hidden) = nodes_of(&s, c, Some(&t));
        assert_eq!(drift_count, 0, "nothing is being read on either side: {drift:?}");
        assert!(drift.is_empty());
        assert!(nodes.iter().all(|n| !n.current), "the record marks no node");
        let book = nodes.iter().find(|n| n.pi_entry.as_deref() == Some("m")).unwrap();
        assert!(book.pi_current, "pi's thread does end there");
        assert!(!book.drift, "and that is pi's own writing, not a disagreement about the conversation");
        assert!(nodes.iter().any(|n| n.pi_entry.as_deref() == Some("a") && !n.on_branch && !n.drift));
    }

    /// The note names the core that answered, and never claims a pi read that did not happen.
    #[test]
    fn the_note_says_which_memory_was_read() {
        let pi_unasked = PiRead { consulted: false, why: "the model process is not holding this conversation".into(), ..Default::default() };
        let n = tree_note("pi", &[ConversationNode { session: 1, title: "t".into(), parent: None, branch_role: String::new(), branch_row: None, branch_quote: String::new(), current: true, parent_gone: false, rows: 2, off_branch: 0, depth: 0, children: 2 }], &pi_unasked, 0, 0, 0, 0);
        assert!(n.contains("pi's tree was not read"), "{n}");
        assert!(!n.contains("its leaf is"), "{n}");

        let pi_read = PiRead { consulted: true, entries: 9, leaf_entry: Some("abc".into()), ..Default::default() };
        let n2 = tree_note("pi", &[ConversationNode { session: 1, title: "t".into(), parent: Some(7), branch_role: "user".into(), branch_row: Some(3), branch_quote: String::new(), current: true, parent_gone: false, rows: 2, off_branch: 3, depth: 1, children: 0 }], &pi_read, 3, 0, 0, 0);
        assert!(n2.contains("pi's thread was read"), "{n2}");
        assert!(n2.contains("its leaf is abc"), "{n2}");
        assert!(n2.contains("cut from conversation 7"), "{n2}");
        assert!(n2.contains("3 rows on branches that are not being read"), "{n2}");

        let n3 = tree_note("rust", &[ConversationNode { session: 1, title: "t".into(), parent: None, branch_role: String::new(), branch_row: None, branch_quote: String::new(), current: true, parent_gone: false, rows: 1, off_branch: 0, depth: 0, children: 0 }], &PiRead::default(), 0, 0, 0, 0);
        assert!(n3.contains("a root"), "{n3}");
        assert!(n3.contains("the record's own node list"), "{n3}");
    }

    /// A host that holds one conversation, for the read and the lever: the tree is the record read
    /// next to what a host's memory says, and neither half alone checks the sentence they make.
    async fn host_holding(store_rows: &[(&str, &str)]) -> (crate::host::SessionHost, Arc<Store>, i64) {
        let (h, store) = crate::host::tests::host_for_test_with_store(vec!["ok".into()]);
        let sess = store.create_session("Roast notes", "/tmp", "m").unwrap();
        for (role, text) in store_rows {
            let m = match *role {
                "user" => Message::user(*text),
                _ => Message::assistant(*text),
            };
            store.append_message(sess, &m).unwrap();
        }
        h.send(crate::host::HostCmd::SwitchSession { id: sess });
        for _ in 0..300 {
            if h.state().session_id == sess {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(h.state().session_id, sess, "the host did not open the conversation");
        (h, store, sess)
    }

    /// The rust core's tree: the record is the whole tree, and the note says so instead of claiming
    /// a pi read this core cannot make.
    #[tokio::test]
    async fn the_rust_core_reads_its_own_record_and_says_that_is_what_it_read() {
        let (h, store, sess) = host_holding(&[
            ("user", "first ask"),
            ("assistant", "first answer"),
            ("user", "second ask"),
            ("assistant", "second answer"),
        ])
        .await;
        let t = read(&*store, &h, sess).await.unwrap();
        assert_eq!(t.core, "rust");
        assert_eq!(t.lineage.len(), 1, "one conversation, drawn as a root");
        assert!(t.lineage[0].parent.is_none());
        assert_eq!(t.nodes.len(), 4);
        assert_eq!(t.nodes.iter().filter(|n| n.on_branch).count(), 4);
        assert!(!t.pi.consulted, "this core has no pi tree");
        assert!(t.drift.is_empty(), "one record cannot disagree with itself");
        assert!(t.note.contains("the record's own node list"), "{}", t.note);
        assert!(t.note.contains("a root"), "{}", t.note);
        assert!(t.nodes.iter().any(|n| n.current), "one of them is the leaf");
    }

    /// Lineage in the read: the child hangs under its parent, at depth 1, quoted at the cut.
    #[tokio::test]
    async fn a_branch_is_drawn_under_the_conversation_it_was_cut_out_of() {
        let (h, store, sess) = host_holding(&[("user", "ask"), ("assistant", "answer")]).await;
        let ids = store.messages(sess).unwrap().into_iter().map(|m| m.id).collect::<Vec<_>>();
        let child = store.create_session("branch", "/tmp", "m").unwrap();
        store.copy_through(sess, child, ids[0]).unwrap();
        store.set_lineage(child, sess, "user", Some(ids[0])).unwrap();
        // Read the *child*: its tree has to show the parent above it, not the sibling beside it.
        let t = read(&*store, &h, child).await.unwrap();
        assert_eq!(t.lineage.len(), 2, "parent then self: {t:?}");
        assert_eq!(t.lineage[0].session, sess);
        assert_eq!(t.lineage[0].depth, 0);
        assert_eq!(t.lineage[1].session, child);
        assert_eq!(t.lineage[1].depth, 1);
        assert_eq!(t.lineage[1].children, 0);
        assert_eq!(t.lineage[0].children, 1, "the parent knows it has a branch");
        assert!(t.lineage[1].branch_quote.contains("your message — ask"), "{}", t.lineage[1].branch_quote);
        assert!(t.note.contains(&format!("cut from conversation {sess}")), "{}", t.note);
    }

    /// The lever on the real host: the record's leaf moves, the model is re-read from that branch,
    /// and the note says which of the two happened.
    #[tokio::test]
    async fn moving_the_leaf_on_the_rust_core_moves_the_record_and_the_model_together() {
        let (h, store, sess) = host_holding(&[
            ("user", "first ask"),
            ("assistant", "first answer"),
            ("user", "second ask"),
            ("assistant", "second answer"),
        ])
        .await;
        let ids = store.messages(sess).unwrap().into_iter().map(|m| m.id).collect::<Vec<_>>();
        let out = move_leaf(
            &*store,
            &h,
            &LeafReq { session: sess, message_id: Some(ids[1]), role: "assistant".into(), shown: "first answer".into(), ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(out.leaf, Some(ids[1]));
        assert_eq!(out.off_branch, 2, "the later two rows are off the path");
        assert!(out.prefill.is_none(), "an answer is not something to ask again");
        assert!(out.reads_through.contains("first answer"), "{}", out.reads_through);
        assert!(out.note.contains("re-read the branch"), "{}", out.note);
        assert!(out.note.contains("2 message"), "{}", out.note);
        assert!(out.note.contains("2 later rows stay in the record"), "{}", out.note);
        assert_eq!(store.history(sess).unwrap().len(), 2, "the prompt the next turn builds from");
        // And the tree now reads the same position the lever left.
        let t = read(&*store, &h, sess).await.unwrap();
        assert_eq!(t.leaf, Some(ids[1]));
        assert_eq!(t.off_branch, 2);
        assert!(t.nodes.iter().any(|n| n.current && n.row == Some(ids[1])));
        assert!(t.note.contains("2 rows on branches that are not being read"), "{}", t.note);
    }

    /// Moving to one of your own messages hands the words back, on the core that re-reads its memory.
    #[tokio::test]
    async fn moving_to_your_own_message_hands_the_words_back_to_the_composer() {
        let (h, store, sess) = host_holding(&[("user", "ask"), ("assistant", "answer")]).await;
        let ids = store.messages(sess).unwrap().into_iter().map(|m| m.id).collect::<Vec<_>>();
        let out = move_leaf(
            &*store,
            &h,
            &LeafReq { session: sess, message_id: Some(ids[0]), role: "user".into(), shown: "ask".into(), ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(out.prefill.as_deref(), Some("ask"));
        assert_eq!(out.leaf, Some(0), "before the first message");
        assert_eq!(store.history(sess).unwrap().len(), 0, "the model reads nothing but its prompt");
        assert_eq!(out.off_branch, 2);
        assert!(out.note.contains("nothing yet"), "{}", out.note);
    }

    /// pi's flat entries, walked: the path being read comes from the **leaf**, so a file with two
    /// paths marks the one being answered and not the one written last.
    ///
    /// This is the shape pi actually answers with (`get_entries`), and the trap is the same one the
    /// nested `get_tree` answer had: the forked entry was appended after the straight-line answer, so
    /// anything that reads the file top-down or takes the newest entry marks `a2` as the conversation
    /// and leaves `a1` — the answer the model is holding — off it.
    #[test]
    fn pis_entries_are_walked_from_the_leaf_and_not_in_file_order() {
        let entries = serde_json::json!([
          {"id": "r", "parentId": null, "type": "model_change", "model": "mock"},
          {"id": "u1", "parentId": "r", "type": "message", "message": {"role": "user", "content": "first ask"}},
          {"id": "a1", "parentId": "u1", "type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "first answer"}]}},
          {"id": "a2", "parentId": "u1", "type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "other path"}]}}
        ]);
        let t = pi_entries_from_json(&entries, Some("a1"), "/tmp/s.jsonl");
        assert_eq!(t.nodes.len(), 4);
        assert_eq!(
            t.nodes.iter().map(|n| n.entry.as_str()).collect::<Vec<_>>(),
            vec!["r", "u1", "a1", "a2"],
            "the list arrives in file order"
        );
        let by = |id: &str| t.nodes.iter().find(|n| n.entry == id).unwrap().clone();
        assert!(by("a1").on_branch);
        assert!(!by("a2").on_branch, "the sibling path is not what the model read, though it is the newer line in the file");
        assert_eq!(by("u1").text, "first ask", "a string content is the entry's words");
        assert_eq!(by("a1").text, "first answer", "a content array is joined");
        assert_eq!(by("r").kind, "model_change");
        assert_eq!(by("r").text, "mock", "pi's own entry is named by what it recorded");
        assert_eq!(by("a1").parent.as_deref(), Some("u1"));
        assert_eq!(t.file, "/tmp/s.jsonl");
        // A leaf nothing in the list answers to marks nothing, rather than guessing the last entry.
        let lost = pi_entries_from_json(&entries, Some("not-here"), "/tmp/s.jsonl");
        assert!(lost.nodes.iter().all(|n| !n.on_branch));
        assert_eq!(lost.leaf.as_deref(), Some("not-here"), "the leaf pi named is still reported");
    }

    /// A thread that points at itself is stopped, not spun.
    #[test]
    fn a_parent_cycle_in_pi_entries_is_walked_once_and_stopped() {
        let entries = serde_json::json!([
          {"id": "u", "parentId": "a", "type": "message", "message": {"role": "user", "content": "ask"}},
          {"id": "a", "parentId": "u", "type": "message", "message": {"role": "assistant", "content": "answer"}}
        ]);
        let t = pi_entries_from_json(&entries, Some("a"), "/tmp/s.jsonl");
        assert!(t.nodes.iter().all(|n| n.on_branch), "both entries were walked");
        assert_eq!(t.leaf.as_deref(), Some("a"));
    }

    /// Deleting a conversation leaves its children pointed at a parent that is gone — and the tree
    /// draws that as a gap, not as a lost child.
    #[test]
    fn deleting_a_parent_leaves_the_child_in_the_tree_with_a_named_gap() {
        let (s, c, ids) = seeded();
        let child = s.create_session("branch", "/tmp", "m").unwrap();
        s.copy_through(c, child, ids[1]).unwrap();
        s.set_lineage(child, c, "user", Some(ids[1])).unwrap();
        s.delete_session(c).unwrap();
        assert_eq!(s.session(child).unwrap().unwrap().parent, Some(c));
        assert!(s.session(c).unwrap().is_none());
        assert_eq!(s.children_of(c).unwrap().len(), 1, "the child is still findable by its parent");
    }

    /// The role rule pi's own navigation uses: your message leaves the read, an answer stays in it.
    #[test]
    fn moving_to_your_message_reads_before_it_and_moving_to_an_answer_reads_through_it() {
        let (s, c, ids) = seeded();
        let all = s.all_rows(c).unwrap();
        let u = resolve(&all, &LeafReq { session: c, message_id: Some(ids[0]), role: "user".into(), shown: "first ask".into(), ..Default::default() }).unwrap();
        assert_eq!(u.leaf, Some(0), "the first message's leaf sits before it, reading nothing");
        s.set_leaf(c, u.leaf).unwrap();
        assert_eq!(s.messages(c).unwrap().len(), 0, "nothing is read, and nothing was deleted");
        assert_eq!(s.message_count(c).unwrap(), 4);
        assert_eq!(s.off_branch_rows(c).unwrap(), 4);
    }
}
