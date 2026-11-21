//! Slot `links-serial`. Every alternative keeps:
//!
//! ```text
//! pub fn build(pairs: &[[Id; 2]]) -> LinkTable
//! ```
//!
//! Joins both IDs of every pair, transitively; each person is named by the byte-wise smallest
//! of their IDs (`types::id::less`). Serial, on one worker beside the parse.
//!
//! Three passes, each prefetching ahead: number the IDs into the final table (a slot's value is
//! the ID's dense index), Rem's union-find with splicing over the dense nodes, linking the
//! larger ID under the smaller, then overwrite each slot's index with its root's ID, so the
//! numbering table is the lookup table and no second one is built.

use std::hint::select_unpredictable;

use types::Id;
use types::id::less;
use types::mem::prefetch;

use crate::{Line, LinkTable};

/// Prefetch distances, each the best of a sweep on the real pairs: pairs ahead when numbering,
/// edges ahead in the unions, lines ahead when naming (twice that for the node, once for its parent).
const NUMBER_AHEAD: usize = 16;
const UNITE_AHEAD: usize = 32;
const NAME_AHEAD: usize = 8;

/// A node's parent, and the parent's ID, so Rem's comparison of two parents reads no other node.
/// A root is its own parent.
#[derive(Clone, Copy)]
struct Node {
    parent_id: Id,
    parent: u32,
}

pub fn build(pairs: &[[Id; 2]]) -> LinkTable {
    assert!(pairs.len() < 1 << 30, "dense indices are u32");
    let mut table = LinkTable::new(2 * pairs.len());
    let mut nodes: Vec<Node> = Vec::with_capacity(2 * pairs.len());
    let mut edges: Vec<[u32; 2]> = Vec::with_capacity(pairs.len());

    for (i, &pair) in pairs.iter().enumerate() {
        if let Some(ahead) = pairs.get(i + NUMBER_AHEAD) {
            for &id in ahead {
                table.prefetch(id);
            }
        }
        // A pair with an empty side links nothing (ingest drops them already), and the empty ID
        // must not reach `number`, where it would match a free slot.
        if pair.contains(&0) {
            continue;
        }
        let [x, y] = pair.map(|id| number(&mut table, id, &mut nodes));
        if x != y {
            edges.push([x, y]);
        }
    }

    for (i, &[x, y]) in edges.iter().enumerate() {
        if let Some(ahead) = edges.get(i + UNITE_AHEAD) {
            for &node in ahead {
                prefetch(nodes.as_ptr().wrapping_add(node as usize));
            }
        }
        unite(&mut nodes, x, y);
    }

    let lines = &mut table.lines;
    for at in 0..lines.len() {
        if let Some(far) = lines.get(at + 2 * NAME_AHEAD) {
            for [_, node] in far.0 {
                prefetch(nodes.as_ptr().wrapping_add(node as usize));
            }
        }
        if let Some(near) = lines.get(at + NAME_AHEAD) {
            for [_, node] in near.0 {
                if let Some(node) = nodes.get(node as usize) {
                    prefetch(nodes.as_ptr().wrapping_add(node.parent as usize));
                }
            }
        }
        for slot in &mut lines[at].0 {
            if slot[0] != 0 {
                slot[1] = name(&mut nodes, slot[1] as u32);
            }
        }
    }
    table
}

/// The dense index of `id`, a new node when first seen.
#[inline(always)]
fn number(table: &mut LinkTable, id: Id, nodes: &mut Vec<Node>) -> u32 {
    let mask = table.lines.len() - 1;
    let mut at = table.home(id);
    loop {
        // SAFETY: `at` is at most `mask`, and the length is a power of two.
        let line: &mut Line = unsafe { table.lines.get_unchecked_mut(at) };
        // The whole line and one branch, as in `LinkTable::person`; the used slots are a prefix.
        let (mut miss, mut index, mut used) = (u64::MAX, 0, 0);
        for [key, value] in line.0 {
            index = select_unpredictable(key == id, value, index);
            miss = miss.min(key ^ id);
            used += usize::from(key != 0);
        }
        if miss == 0 {
            return index as u32;
        }
        if used < 4 {
            let n = nodes.len() as u32;
            line.0[used] = [id, n as Id];
            nodes.push(Node { parent_id: id, parent: n });
            return n;
        }
        at = (at + 1) & mask;
    }
}

/// Rem's algorithm with splicing: walk both paths up, always on the side whose parent has the
/// larger ID, pointing each node passed at the other side's parent, until the parents meet or
/// a root is linked. Every parent has a smaller ID than its child, so a root is its tree's minimum.
#[inline(always)]
fn unite(nodes: &mut [Node], mut x: u32, mut y: u32) {
    loop {
        // SAFETY (all four): every index stored, in edges and parents, is one `number` returned,
        // so is below nodes.len().
        let (nx, ny) = unsafe { (*nodes.get_unchecked(x as usize), *nodes.get_unchecked(y as usize)) };
        if nx.parent == ny.parent {
            return;
        }
        if less(ny.parent_id, nx.parent_id) {
            unsafe { *nodes.get_unchecked_mut(x as usize) = ny };
            if nx.parent == x {
                return;
            }
            x = nx.parent;
        } else {
            unsafe { *nodes.get_unchecked_mut(y as usize) = nx };
            if ny.parent == y {
                return;
            }
            y = ny.parent;
        }
    }
}

/// The root's ID. After the unions 88% of generated nodes are a root or a root's child; a deeper
/// path is pointed straight at the root.
#[inline(always)]
fn name(nodes: &mut [Node], node: u32) -> Id {
    let first = nodes[node as usize];
    if nodes[first.parent as usize].parent == first.parent {
        return first.parent_id;
    }
    let mut root = first.parent;
    loop {
        let parent = nodes[root as usize].parent;
        if parent == root {
            break;
        }
        root = parent;
    }
    let top = nodes[root as usize];
    let mut at = node;
    while at != root {
        let next = nodes[at as usize].parent;
        nodes[at as usize] = top;
        at = next;
    }
    top.parent_id
}
