//! Column mode (D41): a window whose tiled panes sit in columns of their own
//! width, wider than the screen, with each client's view panned to its
//! active pane. Nothing here changes the core: the window is kept by
//! `select-layout` with JSON v2 layouts, tmux hooks and formats.
//!
//! The layout builder is a pure function from the window's layout (and the
//! layout before its last structural change) to the next layout. The
//! commands read the window, build, and write the result in one guarded
//! tmux command so that a change made meanwhile is never overwritten.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The narrowest column.
pub(crate) const MIN_WIDTH: u32 = 10;
/// The widest column window.
pub(crate) const MAX_WINDOW: u32 = 10_000;

/// A tiled pane as read from a layout.
#[derive(Clone, Debug)]
struct Leaf {
    id: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    value: Value,
}

/// Tiled panes that share a left edge, top to bottom.
#[derive(Clone, Debug)]
struct Column {
    width: u32,
    leaves: Vec<Leaf>,
}

/// A window layout split into columns and floating cells.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    width: u32,
    height: u32,
    columns: Vec<Column>,
    /// Floating cells and groups (with `"z"`), whole, wherever they sat.
    floating: Vec<Value>,
    /// The active pane (`"a"`), tiled or floating.
    active: Option<String>,
}

impl Layout {
    /// Reads a `#{window_layout}` JSON v2 value.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| format!("invalid layout: {error}"))?;
        if value["V"] != 2 {
            return Err("invalid layout: not a JSON v2 layout".into());
        }
        let root = &value["L"];
        let mut tiled = Vec::new();
        let mut floating = Vec::new();
        let mut active = None;
        // A window can be left with only floating panes; it has no columns.
        walk(root, &mut tiled, &mut floating, &mut active)?;
        Ok(Self {
            width: number(root, "w")?,
            height: number(root, "h")?,
            columns: columns_of(tiled),
            floating,
            active,
        })
    }

    /// Reads `@masil-columns-prev`: JSON v2, or the old format a hook writes
    /// when the session's latest client is a control client without
    /// `new-layouts`. The old format has only tiled panes, which is all a
    /// prev layout is read for.
    pub(crate) fn parse_prev(text: &str) -> Result<Self, String> {
        if text.starts_with('{') {
            return Self::parse(text);
        }
        let (_, body) = text
            .split_once(',')
            .ok_or("invalid layout: neither JSON nor the old format")?;
        let mut reader = OldFormat {
            bytes: body.as_bytes(),
            at: 0,
        };
        let mut leaves = Vec::new();
        let (width, height) = reader.cell(&mut leaves, 0)?;
        if reader.at != body.len() || leaves.is_empty() {
            return Err("invalid layout: old format".into());
        }
        Ok(Self {
            width,
            height,
            columns: columns_of(leaves),
            floating: Vec::new(),
            active: None,
        })
    }

    /// Tiled panes, the count the layout hook compares.
    pub(crate) fn tiled(&self) -> usize {
        self.columns.iter().map(|column| column.leaves.len()).sum()
    }

    /// Column widths, left to right.
    pub(crate) fn widths(&self) -> Vec<u32> {
        self.columns.iter().map(|column| column.width).collect()
    }

    /// The pane ids of each column, top to bottom.
    pub(crate) fn column_panes(&self) -> Vec<Vec<String>> {
        self.columns
            .iter()
            .map(|column| column.leaves.iter().map(|leaf| leaf.id.clone()).collect())
            .collect()
    }

    /// The column holding a tiled pane.
    pub(crate) fn column_of(&self, pane: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.leaves.iter().any(|leaf| leaf.id == pane))
    }

    /// Every pane, tiled then floating.
    pub(crate) fn panes(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.column_panes().into_iter().flatten().collect();
        for cell in &self.floating {
            leaf_ids(cell, &mut ids);
        }
        ids
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Every pane's geometry, and the window size.
    fn geometry(&self) -> (u32, u32, BTreeMap<String, [u32; 4]>) {
        let mut panes = BTreeMap::new();
        for column in &self.columns {
            for leaf in &column.leaves {
                panes.insert(leaf.id.clone(), [leaf.x, leaf.y, leaf.width, leaf.height]);
            }
        }
        for cell in &self.floating {
            floating_geometry(cell, &mut panes);
        }
        (self.width, self.height, panes)
    }
}

fn number(cell: &Value, key: &str) -> Result<u32, String> {
    cell[key]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("invalid layout: cell without {key}"))
}

/// Tiled panes grouped into columns by left edge, each top to bottom.
fn columns_of(tiled: Vec<Leaf>) -> Vec<Column> {
    let mut by_x: BTreeMap<u32, Vec<Leaf>> = BTreeMap::new();
    for leaf in tiled {
        by_x.entry(leaf.x).or_default().push(leaf);
    }
    by_x.into_values()
        .map(|mut leaves| {
            leaves.sort_by_key(|leaf| leaf.y);
            Column {
                width: leaves.iter().map(|leaf| leaf.width).max().unwrap_or(0),
                leaves,
            }
        })
        .collect()
}

/// A reader for the old layout format: a cell is `WxH,X,Y` followed by
/// `,ID` for a pane or `{...}` / `[...]` for a container of cells.
struct OldFormat<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl OldFormat<'_> {
    fn number(&mut self) -> Result<u32, String> {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(|| "invalid layout: old format number".into())
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.bytes.get(self.at) != Some(&byte) {
            return Err("invalid layout: old format".into());
        }
        self.at += 1;
        Ok(())
    }

    fn cell(&mut self, leaves: &mut Vec<Leaf>, depth: usize) -> Result<(u32, u32), String> {
        if depth > 64 {
            return Err("invalid layout: old format nests too deep".into());
        }
        let width = self.number()?;
        self.expect(b'x')?;
        let height = self.number()?;
        self.expect(b',')?;
        let x = self.number()?;
        self.expect(b',')?;
        let y = self.number()?;
        match self.bytes.get(self.at) {
            Some(&open @ (b'{' | b'[')) => {
                let close = if open == b'{' { b'}' } else { b']' };
                self.at += 1;
                loop {
                    self.cell(leaves, depth + 1)?;
                    if self.bytes.get(self.at) == Some(&b',') {
                        self.at += 1;
                    } else {
                        self.expect(close)?;
                        break;
                    }
                }
            }
            _ => {
                self.expect(b',')?;
                let id = self.number()?;
                leaves.push(Leaf {
                    id: format!("%{id}"),
                    x,
                    y,
                    width,
                    height,
                    value: Value::Null,
                });
            }
        }
        Ok((width, height))
    }
}

fn walk(
    cell: &Value,
    tiled: &mut Vec<Leaf>,
    floating: &mut Vec<Value>,
    active: &mut Option<String>,
) -> Result<(), String> {
    if cell.get("z").is_some() {
        active_in(cell, active);
        floating.push(cell.clone());
        return Ok(());
    }
    match cell["t"].as_str() {
        Some("p") => {
            let id = cell["I"]
                .as_str()
                .filter(|id| crate::pane_id(id).is_ok())
                .ok_or("invalid layout: pane cell without a pane id")?
                .to_owned();
            if cell["a"] == true {
                *active = Some(id.clone());
            }
            tiled.push(Leaf {
                id,
                x: number(cell, "x")?,
                y: number(cell, "y")?,
                width: number(cell, "w")?,
                height: number(cell, "h")?,
                value: cell.clone(),
            });
            Ok(())
        }
        Some("h" | "v") => {
            for child in cell["c"]
                .as_array()
                .ok_or("invalid layout: container without cells")?
            {
                walk(child, tiled, floating, active)?;
            }
            Ok(())
        }
        _ => Err("invalid layout: unknown cell type".into()),
    }
}

fn active_in(cell: &Value, active: &mut Option<String>) {
    if cell["a"] == true
        && let Some(id) = cell["I"].as_str()
    {
        *active = Some(id.to_owned());
    }
    if let Some(children) = cell["c"].as_array() {
        for child in children {
            active_in(child, active);
        }
    }
}

fn leaf_ids(cell: &Value, ids: &mut Vec<String>) {
    match cell["c"].as_array() {
        Some(children) => {
            for child in children {
                leaf_ids(child, ids);
            }
        }
        None => {
            if let Some(id) = cell["I"].as_str() {
                ids.push(id.to_owned());
            }
        }
    }
}

fn floating_geometry(cell: &Value, panes: &mut BTreeMap<String, [u32; 4]>) {
    match cell["c"].as_array() {
        Some(children) => {
            for child in children {
                floating_geometry(child, panes);
            }
        }
        None => {
            if let Some(id) = cell["I"].as_str() {
                let get = |key: &str| cell[key].as_u64().unwrap_or(0) as u32;
                panes.insert(id.to_owned(), [get("x"), get("y"), get("w"), get("h")]);
            }
        }
    }
}

/// What the builder is asked for.
#[derive(Clone, Debug, Default)]
pub(crate) struct Request {
    /// The width of a column that has no width of its own.
    pub default_width: u32,
    /// The window height to lay out; None keeps the current one.
    pub height: Option<u32>,
    /// Widths set by a command, by a pane in the column.
    pub widths: HashMap<String, u32>,
    /// Puts the column of the first pane next to the column of the second:
    /// to its left when `true`.
    pub place: Option<(String, String, bool)>,
}

/// The next layout of a column window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub json: String,
    pub width: u32,
    pub height: u32,
    pub widths: Vec<u32>,
    /// Tiled panes, for `@masil-columns-count`.
    pub count: usize,
    /// Whether the layout differs from the current one.
    pub changed: bool,
}

/// Builds the next layout. A column takes the width it had in `prev`,
/// found by its topmost pane that `prev` holds, unless a column to its left
/// already took that width; any other column is new and takes the default.
/// `prev` None makes every column new (`on`).
pub(crate) fn build(
    current: &Layout,
    prev: Option<&Layout>,
    request: &Request,
) -> Result<Built, String> {
    if current.columns.is_empty() {
        return Err("columns_too_few: the window has no tiled pane".into());
    }
    let mut columns = current.columns.clone();
    if let Some((moving, anchor, before)) = &request.place {
        let from = current
            .column_of(moving)
            .ok_or("invalid_target: the new pane is not tiled")?;
        let column = columns.remove(from);
        let to = columns
            .iter()
            .position(|column| column.leaves.iter().any(|leaf| &leaf.id == anchor))
            .ok_or("invalid_target: the target pane is not tiled")?;
        columns.insert(if *before { to } else { to + 1 }, column);
    }
    let default = request.default_width.max(MIN_WIDTH);
    let mut owner: HashMap<&str, usize> = HashMap::new();
    if let Some(prev) = prev {
        for (index, column) in prev.columns.iter().enumerate() {
            for leaf in &column.leaves {
                owner.insert(leaf.id.as_str(), index);
            }
        }
    }
    let mut taken = HashSet::new();
    let mut widths = Vec::with_capacity(columns.len());
    for column in &columns {
        let set = column
            .leaves
            .iter()
            .find_map(|leaf| request.widths.get(&leaf.id).copied());
        let kept = column
            .leaves
            .iter()
            .find_map(|leaf| owner.get(leaf.id.as_str()).copied())
            .filter(|index| taken.insert(*index))
            .and_then(|index| prev.map(|prev| prev.columns[index].width));
        widths.push(set.or(kept).unwrap_or(default).max(MIN_WIDTH));
    }
    let width = widths.iter().sum::<u32>() + widths.len() as u32 - 1;
    if width > MAX_WINDOW {
        return Err(format!(
            "columns_too_wide: the columns need {width} cells, more than {MAX_WINDOW}"
        ));
    }
    let height = request.height.unwrap_or(current.height);

    // Cells are matched to panes by `"i"` order, so each pane keeps its own
    // rank among every pane of the window.
    let mut ranks = Vec::new();
    for column in &columns {
        for leaf in &column.leaves {
            ranks.push(index_of(&leaf.value)?);
        }
    }
    for cell in &current.floating {
        floating_indexes(cell, &mut ranks)?;
    }
    let mut sorted = ranks.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != ranks.len() {
        return Err("invalid layout: two cells share an index".into());
    }
    let rank: HashMap<u64, usize> = sorted
        .iter()
        .enumerate()
        .map(|(rank, index)| (*index, rank))
        .collect();
    let active = current.active.as_deref();

    let mut cells = Vec::with_capacity(columns.len() + current.floating.len());
    let mut x = 0;
    for (column, column_width) in columns.iter().zip(&widths) {
        let heights = split_heights(column, height)?;
        let mut leaves = Vec::with_capacity(column.leaves.len());
        let mut y = 0;
        for (leaf, leaf_height) in column.leaves.iter().zip(heights) {
            let mut cell = leaf.value.clone();
            let object = cell.as_object_mut().ok_or("invalid layout: pane cell")?;
            object.insert("t".into(), json!("p"));
            object.insert("x".into(), json!(x));
            object.insert("y".into(), json!(y));
            object.insert("w".into(), json!(column_width));
            object.insert("h".into(), json!(leaf_height));
            object.insert("i".into(), json!(rank[&index_of(&leaf.value)?]));
            object.remove("a");
            if active == Some(leaf.id.as_str()) {
                object.remove("l");
            }
            leaves.push(cell);
            y += leaf_height + 1;
        }
        cells.push(if leaves.len() == 1 {
            leaves.pop().unwrap_or_default()
        } else {
            json!({"t":"v","w":column_width,"h":height,"x":x,"y":0,"c":leaves})
        });
        x += column_width + 1;
    }
    let floating_count = current.floating.len();
    for cell in &current.floating {
        let mut cell = cell.clone();
        let (x, y, w, h) = (
            number(&cell, "x").unwrap_or(0),
            number(&cell, "y").unwrap_or(0),
            number(&cell, "w").unwrap_or(0),
            number(&cell, "h").unwrap_or(0),
        );
        let dx = i64::from(pull_in(x, w, width)) - i64::from(x);
        let dy = i64::from(pull_in(y, h, height)) - i64::from(y);
        renumber_floating(&mut cell, &rank, active, dx, dy)?;
        cells.push(cell);
    }
    let root = if cells.len() == 1 && floating_count == 0 {
        cells.pop().unwrap_or_default()
    } else {
        json!({"t":"h","w":width,"h":height,"x":0,"y":0,"c":cells})
    };
    let json = serde_json::to_string(&json!({"V":2,"L":root}))
        .map_err(|error| format!("layout: {error}"))?;
    let next = Layout::parse(&json)?;
    Ok(Built {
        changed: next.geometry() != current.geometry(),
        json,
        width,
        height,
        widths,
        count: current.tiled(),
    })
}

fn index_of(cell: &Value) -> Result<u64, String> {
    cell["i"]
        .as_u64()
        .ok_or_else(|| "invalid layout: pane cell without an index".into())
}

fn floating_indexes(cell: &Value, ranks: &mut Vec<u64>) -> Result<(), String> {
    match cell["c"].as_array() {
        Some(children) => {
            for child in children {
                floating_indexes(child, ranks)?;
            }
            Ok(())
        }
        None => {
            ranks.push(index_of(cell)?);
            Ok(())
        }
    }
}

/// A floating cell's position, moved inside a window of `limit` cells.
fn pull_in(position: u32, size: u32, limit: u32) -> u32 {
    if position + size < limit {
        position
    } else {
        limit.saturating_sub(size + 1).max(1).min(position)
    }
}

fn renumber_floating(
    cell: &mut Value,
    rank: &HashMap<u64, usize>,
    active: Option<&str>,
    dx: i64,
    dy: i64,
) -> Result<(), String> {
    let object: &mut Map<String, Value> = cell
        .as_object_mut()
        .ok_or("invalid layout: floating cell")?;
    for (key, delta) in [("x", dx), ("y", dy)] {
        if let Some(value) = object.get(key).and_then(Value::as_i64) {
            object.insert(key.into(), json!((value + delta).max(0)));
        }
    }
    object.remove("a");
    if active.is_some() && object.get("I").and_then(Value::as_str) == active {
        object.remove("l");
    }
    if let Some(children) = object.get_mut("c").and_then(Value::as_array_mut) {
        for child in children {
            renumber_floating(child, rank, active, dx, dy)?;
        }
    } else {
        let index = object
            .get("i")
            .and_then(Value::as_u64)
            .ok_or("invalid layout: pane cell without an index")?;
        object.insert("i".into(), json!(rank[&index]));
    }
    Ok(())
}

/// The heights of a column's panes in a window `height` tall: the current
/// heights when they fill it, otherwise shared in proportion, at least one
/// row each.
fn split_heights(column: &Column, height: u32) -> Result<Vec<u32>, String> {
    let count = column.leaves.len() as u32;
    let borders = count - 1;
    let current: Vec<u32> = column.leaves.iter().map(|leaf| leaf.height).collect();
    if current.iter().sum::<u32>() + borders == height {
        return Ok(current);
    }
    let room = height
        .checked_sub(borders)
        .filter(|room| *room >= count)
        .ok_or_else(|| {
            format!("columns_too_short: {count} stacked panes do not fit in {height} rows")
        })?;
    let total = current.iter().map(|h| u64::from(*h)).sum::<u64>().max(1);
    let mut heights: Vec<u32> = current
        .iter()
        .map(|h| ((u64::from(*h) * u64::from(room)) / total).max(1) as u32)
        .collect();
    // The last pane takes what is left; when the floors overshoot, the
    // tallest panes give rows back.
    let last = heights.len() - 1;
    loop {
        let others: u32 = heights[..last].iter().sum();
        if others < room {
            heights[last] = room - others;
            break;
        }
        let tallest = (0..last)
            .max_by_key(|index| heights[*index])
            .filter(|index| heights[*index] > 1)
            .ok_or("columns_too_short: no room")?;
        heights[tallest] -= 1;
    }
    Ok(heights)
}

/// What `list-clients` reads for each client.
pub(crate) const CLIENT_FORMAT: &str = "#{client_name}\t#{client_control_mode}\t#{client_flags}\t#{window_id}\t#{client_width}\t#{client_height}\t#{status}\t#{status-position}\t#{@masil-status-width}\t#{?window_offset_x,#{window_offset_x},0}\t#{?window_offset_y,#{window_offset_y},0}";

/// A terminal client and the window area it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Client {
    pub name: String,
    /// The window it shows.
    pub window: String,
    /// Its window area, without a status line or column.
    pub width: u32,
    pub height: u32,
    /// Whether its size counts (not control mode, not `ignore-size`).
    pub sizing: bool,
    /// Where its view starts in the window.
    pub x: u32,
    pub y: u32,
}

/// Clients from `list-clients -F CLIENT_FORMAT`; lines that do not parse
/// are left out.
pub(crate) fn parse_clients(text: &str) -> Vec<Client> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [
                name,
                control,
                flags,
                window,
                width,
                height,
                status,
                position,
                column,
                x,
                y,
            ] = fields.as_slice()
            else {
                return None;
            };
            let (width, height) = area(
                width.parse().ok()?,
                height.parse().ok()?,
                status,
                position,
                column,
            );
            Some(Client {
                name: (*name).to_owned(),
                window: (*window).to_owned(),
                width,
                height,
                sizing: *control == "0" && !flags.split(',').any(|flag| flag == "ignore-size"),
                x: x.parse().unwrap_or(0),
                y: y.parse().unwrap_or(0),
            })
        })
        .collect()
}

/// A client's window area, as the core's `status_line_size` and
/// `status_column_size` take the status line or column off it.
fn area(width: u32, height: u32, status: &str, position: &str, column: &str) -> (u32, u32) {
    let lines = match status {
        "off" => 0,
        "on" => 1,
        other => other.parse().unwrap_or(1),
    };
    // A terminal no taller than the status shows no status.
    if lines == 0 || height <= lines {
        return (width, height);
    }
    if matches!(position, "left" | "right") {
        let size = column
            .parse::<u32>()
            .ok()
            .filter(|size| (8..=80).contains(size))
            .unwrap_or(24);
        let taken = if width < size + 10 { 0 } else { size };
        (width - taken, height)
    } else {
        (width, height - lines)
    }
}

/// The client a window is sized for: the named one, or the narrowest one
/// showing the window whose size counts.
pub(crate) fn base_client<'a>(
    clients: &'a [Client],
    window: &str,
    named: Option<&str>,
) -> Option<&'a Client> {
    match named {
        Some(name) => clients.iter().find(|client| client.name == name),
        None => clients
            .iter()
            .filter(|client| client.sizing && client.window == window)
            .min_by_key(|client| (client.width, client.name.clone())),
    }
}

/// The height a column window takes: the lowest area of the clients
/// showing it whose size counts; None when no such client shows it.
pub(crate) fn window_height(clients: &[Client], window: &str) -> Option<u32> {
    clients
        .iter()
        .filter(|client| client.sizing && client.window == window)
        .map(|client| client.height)
        .min()
}

/// The hook slot every column hook takes.
pub(crate) const SLOT: &str = "4141";
/// The text that marks a hook in the slot as ours, however the core prints
/// it back.
pub(crate) const MARK: &str = "@masil-columns";
const AGENT: &str = "#{q:@masil-columns-agent}";
const SOCKET: &str = "#{q:socket_path}";
/// The offset of a client's view into its window.
const OFFSET: &str = "#{?window_offset_x,#{window_offset_x},0}";
const VIEW: &str = "#{masil_viewport_width}";
/// The panes' cells in pane-list order (`/i`; a plain `#{P:}` goes by
/// creation), without the panes. `swap-pane` swaps two panes' list places
/// and cells together (core/cmd-swap-pane.c), so it leaves this as it was;
/// a drag or any other resize changes it.
const CELLS: &str = "#{P/i:#{pane_left}.#{pane_top}.#{pane_width}.#{pane_height}:}";
/// The monitor that warns about `synchronize-panes` in a column window.
pub(crate) const SYNC_MONITOR: &str = "@masil-columns-sync";
/// Global options column mode keeps.
pub(crate) const GLOBALS: [&str; 4] = [
    "@masil-columns-agent",
    "@masil-columns-pending",
    "@masil-columns-offscreen",
    SYNC_MONITOR,
];

/// Starts one helper for every column window within 0.15 s, however many
/// events ask. Its output and exit status go nowhere; a failed start clears
/// the mark so the next event tries again.
fn resync() -> String {
    format!(
        "if -F \"#{{@masil-columns-pending}}\" {{}} {{ set -g @masil-columns-pending 1 ; if-shell -b \"sleep 0.15; exec {AGENT} layout --socket {SOCKET} columns sync --all\" {{}} {{ set -gu @masil-columns-pending }} }}"
    )
}

/// The view offset that shows a client's active pane whole.
fn target() -> String {
    format!(
        "#{{?#{{e|<:#{{pane_left}},{OFFSET}}},#{{pane_left}},#{{?#{{e|>=:#{{pane_right}},#{{e|+:{OFFSET},{VIEW}}}}},#{{e|+:#{{e|-:#{{pane_right}},{VIEW}}},1}},{OFFSET}}}}}"
    )
}

/// For one client in a `#{L:}` loop: pans its view of a column window wider
/// than the view to the active pane, or after the cursor when the pane is
/// wider than the view. Inside `#{L:}` the window and pane formats are the
/// ones of the loop's window, not of the client's (format.c
/// `format_loop_clients`), so each caller picks the clients that show it.
fn pan_each() -> String {
    let target = target();
    format!(
        "#{{?#{{&&:#{{@masil-columns}},#{{e|>:#{{window_width}},{VIEW}}}}},#{{?#{{e|>:#{{pane_width}},{VIEW}}},refresh-client -t #{{q:client_name}} -c ; ,refresh-client -t #{{q:client_name}} -L 65535 ; #{{?#{{e|>:{target},0}},refresh-client -t #{{q:client_name}} -R {target} ; ,}}}},}}"
    )
}

/// The focus hook's pan: the clients that show the hook's window (in its
/// session, with it current). Formats only; no process.
pub(crate) fn pan_window() -> String {
    format!(
        "run-shell -C \"#{{L:#{{?#{{&&:#{{==:#{{client_session}},#{{session_name}}}},#{{window_active}}}},{},}}}}\"",
        pan_each()
    )
}

/// Every client's pan, each by its own session's current window and active
/// pane: the format `run-shell -C` takes.
pub(crate) fn pan_all() -> String {
    format!(
        "#{{L:#{{S:#{{?#{{==:#{{session_name}},#{{client_session}}}},{},}}}}}}",
        pan_each()
    )
}

/// What `@masil-columns-place` keeps: the cells, the active pane and its
/// left edge.
fn place() -> String {
    format!("{CELLS}|#{{pane_id}}|#{{pane_left}}")
}

/// The hooks column mode installs, by name, all in `SLOT`.
pub(crate) fn hooks() -> Vec<(&'static str, String)> {
    let resync = resync();
    let tiled = "#{n:#{P:#{?pane_floating_flag,,x}}}";
    let clients = format!(
        "if -F \"#{{&&:#{{@masil-columns-agent}},#{{S:#{{W:#{{?#{{@masil-columns}},1,}}}}}}}}\" {{ {resync} }}"
    );
    let count_changed =
        format!("#{{&&:#{{@masil-columns-agent}},#{{!=:{tiled},#{{@masil-columns-count}}}}}}");
    let keep =
        "if -F \"#{@masil-columns-dirty}\" {} { set -wF @masil-columns-prev \"#{window_layout}\" }";
    let pan = pan_window();
    let place = place();
    let keep_place = format!("set -wF @masil-columns-place \"{place}\"");
    // The cells and the active pane are as kept but the pane is elsewhere:
    // a `swap-pane` that kept the active pane active (the `{` and `}` keys)
    // moved it, and no focus hook ran.
    let swapped = format!(
        "#{{&&:#{{m:{CELLS}|#{{pane_id}}|*,#{{@masil-columns-place}}}},#{{!=:#{{@masil-columns-place}},{place}}}}}"
    );
    let mut hooks = vec![
        // `rotate-window` moves panes through the cells and fires no layout
        // hook (core/cmd-rotate-window.c), only this one when it changes the
        // active pane (not with a modal pane), so this one keeps the layout
        // as prev too.
        (
            "window-pane-changed",
            format!(
                "if -F \"#{{@masil-columns}}\" {{ {pan} ; {keep_place} ; if -F \"{count_changed}\" {{}} {{ {keep} }} }}"
            ),
        ),
        // A change of the tiled count starts a helper and marks the window
        // dirty; until the helper's apply clears the mark, no layout is kept
        // as prev, so a count that comes back (a kill, then a split) does not
        // keep the layout tmux made meanwhile.
        (
            "window-layout-changed",
            format!(
                "if -F \"#{{@masil-columns}}\" {{ if -F \"{count_changed}\" {{ set -w @masil-columns-dirty 1 ; {resync} }} {{ if -F \"{swapped}\" {{ {pan} }} ; {keep_place} ; {keep} }} }}"
            ),
        ),
        (
            "window-zoomed",
            format!(
                "if -F \"#{{&&:#{{@masil-columns}},#{{window_zoomed_flag}}}}\" {{ resize-pane -Z ; if -F \"#{{&&:#{{@masil-columns-agent}},#{{?pane_floating_flag,,1}}}}\" {{ if-shell -b \"exec {AGENT} layout --socket {SOCKET} columns fit -t #{{pane_id}}\" {{}} }} }}"
            ),
        ),
    ];
    // `if-shell -b` queues its else on the hook's client; when that client
    // left before a failed helper ended, pending stays set. An attach
    // clears it.
    hooks.push((
        "client-attached",
        format!("set -gu @masil-columns-pending ; {clients}"),
    ));
    for name in [
        "client-detached",
        "client-session-changed",
        "client-resized",
        "session-window-changed",
    ] {
        hooks.push((name, clients.clone()));
    }
    let mut cleanup: Vec<String> = hooks
        .iter()
        .map(|(name, _)| format!("set-hook -gu {name}[{SLOT}]"))
        .collect();
    cleanup.push(format!("set-hook -gu window-closed[{SLOT}]"));
    cleanup.extend(GLOBALS.iter().map(|option| format!("set -gu {option}")));
    hooks.push((
        "window-closed",
        format!(
            "if -F \"#{{S:#{{W:#{{?#{{@masil-columns}},1,}}}}}}\" {{}} {{ {} }}",
            cleanup.join(" ; ")
        ),
    ));
    hooks
}

/// The `synchronize-panes` monitor: its `-B` value and its command.
pub(crate) fn sync_monitor() -> (String, &'static str) {
    (
        format!(
            "{SYNC_MONITOR}::#{{S:#{{W:#{{?#{{&&:#{{@masil-columns}},#{{synchronize-panes}}}},#{{window_id}}.,}}}}}}"
        ),
        "display-message \"masil: synchronize-panes in a column window also types into off-screen columns\"",
    )
}

/// The format a user puts in a status line: how many panes off its view
/// need attention, for the client that draws it.
pub(crate) fn offscreen() -> String {
    format!(
        "#{{n:#{{P:#{{?#{{&&:#{{@masil-attention}},#{{||:#{{e|<:#{{pane_left}},{OFFSET}}},#{{e|>=:#{{pane_right}},#{{e|+:{OFFSET},{VIEW}}}}}}}}},x,}}}}}}"
    )
}

/// The panes' places and active and last marks, in creation order, that an
/// apply checks first.
pub(crate) const SIGNATURE: &str = "#{P:#{pane_id}.#{pane_left}.#{pane_top}.#{pane_width}.#{pane_height}.#{pane_active}#{pane_last}:}";

/// Runs `list` on a window only while its panes are where they were read
/// (`signature`, from `SIGNATURE`); prints `columns_applied` or
/// `columns_stale`.
pub(crate) fn guarded(window: &str, signature: &str, list: &[String]) -> Vec<String> {
    let mut commands = list.to_vec();
    commands.push("display-message -p columns_applied".into());
    vec![
        "if-shell".into(),
        "-F".into(),
        "-t".into(),
        window.into(),
        format!("#{{==:{SIGNATURE},{signature}}}"),
        commands.join(" ; "),
        "display-message -p columns_stale".into(),
    ]
}

/// Quotes a value for a tmux command list in single quotes.
pub(crate) fn single_quoted(value: &str) -> Result<String, String> {
    if value.contains('\'') || value.chars().any(char::is_control) {
        return Err("columns: a value cannot be quoted for tmux".into());
    }
    Ok(format!("'{value}'"))
}

// ---------------------------------------------------------------- commands

const USAGE: &str = "usage: masil-agent layout [--socket S] columns SUB; SUB is one of\n  on [-t WINDOW] [--width N] [--client NAME]\n  off [-t WINDOW]\n  add [-t PANE] [--width N] [--before] [-d] [-c CWD] [-- COMMAND...]\n  close [-t PANE] [--force]\n  width N|+N|-N|N% [-t PANE] [--client NAME]\n  fit [-t PANE] [--client NAME]\n  float [-t WINDOW] [-x W] [-y H] [--client NAME] [-c CWD] [-- COMMAND...]\n  sync [--window WINDOW | --all]\n  show [-t WINDOW]";
/// Attempts at an apply that finds the window changed since it was read.
const STALE_ATTEMPTS: usize = 3;
/// Window options column mode keeps on a window.
const WINDOW_OPTIONS: [&str; 7] = [
    MARK,
    "@masil-columns-count",
    "@masil-columns-dirty",
    "@masil-columns-prev",
    "@masil-columns-fit",
    "@masil-columns-saved",
    "@masil-columns-place",
];
/// What the window read asks for, tab separated. The target pane's fields
/// are the active pane's when the target is a window.
const WINDOW_FORMAT: &str = "#{window_id}\t#{window_layout}\t#{window_zoomed_flag}\t#{@masil-columns}\t#{@masil-columns-count}\t#{@masil-columns-dirty}\t#{@masil-columns-prev}\t#{@masil-columns-fit}\t#{@masil-columns-saved}\t#{@masil-columns-width}\t#{P:#{pane_id}.#{pane_left}.#{pane_top}.#{pane_width}.#{pane_height}.#{pane_active}#{pane_last}:}\t#{pane_id}\t#{pane_floating_flag}\t#{?#{@masil-managed-agent},1,0}\t#{synchronize-panes}\t#{window_width}";

pub(crate) fn run(socket: Option<std::path::PathBuf>, args: &[String]) -> Result<i32, String> {
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let socket = socket
        .ok_or("server_unreachable: no masil server; pass --socket or run inside masil")?
        .canonicalize()
        .map_err(|error| format!("server_unreachable: native socket: {error}"))?;
    let server = Server {
        native: crate::native_ui::Context {
            socket,
            client: None,
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let value = runtime.block_on(command(&server, &args[0], &args[1..]))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(0)
}

/// Column mode for session restore and layout templates.
pub(crate) struct Columns {
    server: Server,
}

impl Columns {
    pub(crate) fn new(socket: std::path::PathBuf) -> Self {
        Self {
            server: Server {
                native: crate::native_ui::Context {
                    socket,
                    client: None,
                },
            },
        }
    }

    /// The width a new column of WINDOW takes: WIDTH if given, else the
    /// window's, the global `@masil-columns-width`, or half the screen of
    /// the narrowest client that views it (80 without one).
    pub(crate) async fn width_for(&self, window: &str, width: Option<u32>) -> Result<u32, String> {
        if let Some(width) = width {
            return cells("--width", &width.to_string());
        }
        let state = State::read(&self.server, Some(window)).await?;
        let clients = self.server.clients().await?;
        Ok(default_width(
            None,
            &state,
            base_client(&clients, &state.window, None),
        ))
    }

    /// `layout columns on -t WINDOW [--width WIDTH]`.
    pub(crate) async fn turn_on(&self, window: &str, width: Option<u32>) -> Result<Value, String> {
        let options = Options {
            target: Some(window.to_owned()),
            width,
            ..Options::default()
        };
        on(&self.server, &options).await
    }

    /// Puts a restored window back in column mode. Its saved layout already
    /// has the widths, so the layout read now is the prev. `off` later keeps
    /// the layout and clears the manual size the restore set. An error from
    /// the sync, or a layout it could not write, is an error here.
    pub(crate) async fn restored(&self, window: &str) -> Result<(), String> {
        check_slots(&self.server).await?;
        let saved = json!({"window_size": null, "layout": null}).to_string();
        let list = [[MARK, "1"], ["@masil-columns-saved", saved.as_str()]]
            .into_iter()
            .map(|[option, value]| {
                ["set", "-w", "-t", window, option, value]
                    .map(str::to_owned)
                    .to_vec()
            })
            .chain([[
                "set",
                "-wF",
                "-t",
                window,
                "@masil-columns-prev",
                "#{window_layout}",
            ]
            .map(str::to_owned)
            .to_vec()])
            .collect();
        self.server.run(&joined(list)).await?;
        let options = Options {
            window: Some(window.to_owned()),
            ..Options::default()
        };
        let report = sync(&self.server, &options).await?;
        sync_failure(&report["windows"][0]).map_or(Ok(()), Err)
    }

    /// Takes a window a restore could not put back out of column mode: no
    /// column options, a size that follows the client again, and no hooks
    /// when no other window is in column mode.
    pub(crate) async fn abandon(&self, window: &str) -> Result<(), String> {
        clear_window(&self.server, window).await?;
        restore_size(&self.server, window, &None).await?;
        if self.server.column_windows().await?.is_empty() {
            uninstall(&self.server).await?;
        }
        Ok(())
    }
}

/// Why a window's entry in a `sync` report is not in column mode, if it is
/// not: an error, or a layout the write could not apply.
fn sync_failure(entry: &Value) -> Option<String> {
    if entry.is_null() {
        return Some("columns: the window is not in column mode".into());
    }
    if let Some(error) = entry["error"].as_str() {
        return Some(error.to_owned());
    }
    entry["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|warning| warning.starts_with("columns_apply_failed"))
        .map(str::to_owned)
}

/// Runs a column call for callers outside a runtime.
fn blocking<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(future)
}

/// `Columns::restored` for callers outside a runtime.
pub(crate) fn restored_blocking(socket: &std::path::Path, window: &str) -> Result<(), String> {
    blocking(Columns::new(socket.to_path_buf()).restored(window))
}

/// `Columns::abandon` for callers outside a runtime.
pub(crate) fn abandon_blocking(socket: &std::path::Path, window: &str) -> Result<(), String> {
    blocking(Columns::new(socket.to_path_buf()).abandon(window))
}

async fn command(server: &Server, name: &str, args: &[String]) -> Result<Value, String> {
    match name {
        "on" => on(server, &options(args, &["-t", "--width", "--client"])?).await,
        "off" => off(server, &options(args, &["-t"])?).await,
        "add" => {
            let allowed = ["-t", "--width", "--before", "-d", "-c", "--"];
            add(server, &options(args, &allowed)?).await
        }
        "close" => close(server, &options(args, &["-t", "--force"])?).await,
        "width" => {
            let value = args.first().ok_or_else(|| USAGE.to_owned())?;
            let change = WidthChange::parse(value)?;
            width(server, change, &options(&args[1..], &["-t", "--client"])?).await
        }
        "fit" => fit(server, &options(args, &["-t", "--client"])?).await,
        "float" => {
            let allowed = ["-t", "-x", "-y", "--client", "-c", "--"];
            float(server, &options(args, &allowed)?).await
        }
        "sync" => {
            let options = options(args, &["--window", "--all"])?;
            if options.window.is_some() == options.all {
                return Err(format!(
                    "usage: sync takes --window WINDOW or --all\n{USAGE}"
                ));
            }
            sync(server, &options).await
        }
        "show" => show(server, &options(args, &["-t"])?).await,
        _ => Err(USAGE.into()),
    }
}

#[derive(Debug, Default)]
struct Options {
    target: Option<String>,
    width: Option<u32>,
    client: Option<String>,
    cwd: Option<String>,
    size: (Option<u32>, Option<u32>),
    window: Option<String>,
    before: bool,
    detached: bool,
    force: bool,
    all: bool,
    command: Option<Vec<String>>,
}

fn options(args: &[String], allowed: &[&str]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut seen = HashSet::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !allowed.contains(&arg) || !seen.insert(arg) {
            return Err(format!("usage: unknown or repeated option {arg}\n{USAGE}"));
        }
        if arg == "--" {
            let command = args[index + 1..].to_vec();
            if command.is_empty() || command.iter().any(String::is_empty) {
                return Err(format!("usage: -- needs a command\n{USAGE}"));
            }
            crate::layout::validate_argv(&command).map_err(|error| format!("usage: {error}"))?;
            options.command = Some(command);
            break;
        }
        let flag = matches!(arg, "--before" | "-d" | "--force" | "--all");
        if flag {
            match arg {
                "--before" => options.before = true,
                "-d" => options.detached = true,
                "--force" => options.force = true,
                _ => options.all = true,
            }
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("usage: missing value for {arg}\n{USAGE}"))?
            .clone();
        match arg {
            "-t" => options.target = Some(value),
            "--width" => options.width = Some(cells(arg, &value)?),
            "--client" => options.client = Some(value),
            "-c" => options.cwd = Some(directory(&value)?),
            "-x" => options.size.0 = Some(cells(arg, &value)?),
            "-y" => options.size.1 = Some(cells(arg, &value)?),
            _ => options.window = Some(value),
        }
        index += 2;
    }
    Ok(options)
}

/// A `-c` directory as an absolute path tmux takes literally: its `-c`
/// expands formats, so no `#`.
fn directory(value: &str) -> Result<String, String> {
    let resolved = std::path::Path::new(value)
        .canonicalize()
        .map_err(|error| format!("usage: -c {value}: {error}"))?;
    if !resolved.is_dir() {
        return Err(format!("usage: -c {value} is not a directory"));
    }
    let text = resolved
        .to_str()
        .filter(|text| !text.contains('#') && !text.chars().any(char::is_control))
        .ok_or_else(|| {
            format!("usage: -c {value} must be UTF-8 without '#' or control characters")
        })?;
    Ok(text.to_owned())
}

fn cells(option: &str, value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|cells| (1..=MAX_WINDOW).contains(cells))
        .ok_or_else(|| format!("usage: {option} takes a number of cells from 1 to {MAX_WINDOW}"))
}

/// A `width` command's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WidthChange {
    Set(u32),
    Wider(u32),
    Narrower(u32),
    /// A share of the base client's view.
    Percent(u32),
}

impl WidthChange {
    fn parse(value: &str) -> Result<Self, String> {
        let number = |text: &str| {
            text.parse::<u32>()
                .ok()
                .filter(|n| (1..=MAX_WINDOW).contains(n))
        };
        let change = if let Some(rest) = value.strip_suffix('%') {
            number(rest)
                .filter(|percent| *percent <= 100)
                .map(Self::Percent)
        } else if let Some(rest) = value.strip_prefix('+') {
            number(rest).map(Self::Wider)
        } else if let Some(rest) = value.strip_prefix('-') {
            number(rest).map(Self::Narrower)
        } else {
            number(value).map(Self::Set)
        };
        change.ok_or_else(|| format!("usage: width takes N, +N, -N or N% (1-100), not {value}"))
    }

    fn apply(self, current: u32, view: Option<u32>) -> Option<u32> {
        Some(match self {
            Self::Set(width) => width,
            Self::Wider(by) => current.saturating_add(by),
            Self::Narrower(by) => current.saturating_sub(by),
            Self::Percent(percent) => view? * percent / 100,
        })
    }
}

struct Server {
    native: crate::native_ui::Context,
}

impl Server {
    async fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<String, String> {
        let output = self
            .native
            .tmux(
                args.iter()
                    .map(|arg| std::ffi::OsString::from(arg.as_ref())),
                None,
            )
            .await
            .map_err(|error| {
                if error.starts_with("native command failed")
                    || error.starts_with("native command exited")
                {
                    error
                } else {
                    format!("server_unreachable: {error}")
                }
            })?;
        String::from_utf8(output.stdout).map_err(|_| "native response is not UTF-8".into())
    }

    async fn clients(&self) -> Result<Vec<Client>, String> {
        Ok(parse_clients(
            &self.run(&["list-clients", "-F", CLIENT_FORMAT]).await?,
        ))
    }

    /// The windows in column mode, each once.
    async fn column_windows(&self) -> Result<Vec<String>, String> {
        let text = self
            .run(&[
                "list-windows",
                "-a",
                "-F",
                "#{window_id}\t#{@masil-columns}",
            ])
            .await?;
        let mut windows = Vec::new();
        for line in text.lines() {
            if let Some((window, "1")) = line.split_once('\t')
                && !windows.iter().any(|known| known == window)
            {
                windows.push(window.to_owned());
            }
        }
        Ok(windows)
    }
}

/// A column window as read in one native command.
#[derive(Clone, Debug)]
struct State {
    window: String,
    /// `#{window_layout}`: the layout before any zoom.
    text: String,
    layout: Layout,
    zoomed: bool,
    on: bool,
    count: Option<usize>,
    dirty: bool,
    prev: String,
    fit: String,
    saved: String,
    width_option: Option<u32>,
    signature: String,
    pane: String,
    pane_floating: bool,
    pane_agent: bool,
    synchronize: bool,
    width: u32,
}

impl State {
    async fn read(server: &Server, target: Option<&str>) -> Result<Self, String> {
        let mut args = vec!["display-message", "-p"];
        if let Some(target) = target {
            args.extend(["-t", target]);
        }
        args.push(WINDOW_FORMAT);
        let text = server.run(&args).await?;
        let line = text.strip_suffix('\n').unwrap_or(&text);
        let fields: Vec<&str> = line.split('\t').collect();
        // For an unknown target the read succeeds with its fields empty.
        if !fields.first().is_some_and(|window| window.starts_with('@')) {
            return Err(format!(
                "invalid_target: {} names no window",
                target.unwrap_or("the current pane")
            ));
        }
        let [
            window,
            layout,
            zoomed,
            on,
            count,
            dirty,
            prev,
            fit,
            saved,
            width_option,
            signature,
            pane,
            floating,
            agent,
            synchronize,
            width,
        ] = fields.as_slice()
        else {
            return Err("columns: the window read does not parse".into());
        };
        Ok(Self {
            window: (*window).to_owned(),
            text: (*layout).to_owned(),
            layout: Layout::parse(layout)?,
            zoomed: *zoomed == "1",
            on: *on == "1",
            count: count.parse().ok(),
            dirty: !dirty.is_empty(),
            prev: (*prev).to_owned(),
            fit: (*fit).to_owned(),
            saved: (*saved).to_owned(),
            width_option: width_option
                .parse()
                .ok()
                .filter(|width| (MIN_WIDTH..=MAX_WINDOW).contains(width)),
            signature: (*signature).to_owned(),
            pane: (*pane).to_owned(),
            pane_floating: *floating == "1",
            pane_agent: *agent == "1",
            synchronize: *synchronize == "1",
            width: width.parse().unwrap_or(0),
        })
    }

    fn require_on(&self) -> Result<(), String> {
        if self.on {
            Ok(())
        } else {
            Err(format!(
                "invalid_target: window {} is not in column mode; run layout columns on",
                self.window
            ))
        }
    }

    fn require_tiled(&self) -> Result<(), String> {
        if self.pane_floating {
            Err(format!("invalid_target: {} is floating", self.pane))
        } else {
            Ok(())
        }
    }

    /// The layout widths are kept from: `@masil-columns-prev`, or the
    /// current layout with a warning when it does not parse.
    fn prev(&self, warnings: &mut Vec<String>) -> Layout {
        match Layout::parse_prev(&self.prev) {
            Ok(prev) => prev,
            Err(_) => {
                if !self.prev.is_empty() {
                    warnings.push("columns_prev_unreadable".into());
                }
                self.layout.clone()
            }
        }
    }

    /// The prev of `add` and `close`: their own first read, unless a change
    /// the helper has not handled yet is in it.
    fn prev_before_change(&self, warnings: &mut Vec<String>) -> Layout {
        if self.dirty || self.count != Some(self.layout.tiled()) {
            self.prev(warnings)
        } else {
            self.layout.clone()
        }
    }

    /// `@masil-columns-fit`, without panes that are gone.
    fn fit(&self, warnings: &mut Vec<String>) -> BTreeMap<String, u32> {
        if self.fit.is_empty() {
            return BTreeMap::new();
        }
        match serde_json::from_str::<BTreeMap<String, u32>>(&self.fit) {
            Ok(mut fit) => {
                fit.retain(|pane, _| self.layout.column_of(pane).is_some());
                fit
            }
            Err(_) => {
                warnings.push("columns_fit_reset".into());
                BTreeMap::new()
            }
        }
    }
}

fn encode_fit(fit: &BTreeMap<String, u32>) -> String {
    if fit.is_empty() {
        String::new()
    } else {
        serde_json::to_string(fit).unwrap_or_default()
    }
}

/// The width a new column takes.
fn default_width(asked: Option<u32>, state: &State, base: Option<&Client>) -> u32 {
    asked
        .or(state.width_option)
        .unwrap_or_else(|| base.map_or(80, |client| (client.width / 2).max(20)))
}

/// The base client a command names or finds; a named client must exist.
fn base<'a>(
    clients: &'a [Client],
    window: &str,
    named: Option<&str>,
) -> Result<Option<&'a Client>, String> {
    let found = base_client(clients, window, named);
    if let (Some(name), None) = (named, found) {
        return Err(format!("invalid_target: no attached client {name}"));
    }
    Ok(found)
}

fn require_base<'a>(
    clients: &'a [Client],
    window: &str,
    named: Option<&str>,
) -> Result<&'a Client, String> {
    base(clients, window, named)?
        .ok_or_else(|| format!("client_ambiguous: no client views {window}; pass --client NAME"))
}

/// What a command asks to write to a window.
enum Change {
    Layout {
        built: Built,
        fit: String,
        /// Commands that go in the same guarded list, before the options.
        extra: Vec<String>,
    },
    /// The layout cannot be made (too short, too wide): the count and prev
    /// are written so the layout hook stops asking.
    Skip { count: usize },
    /// Nothing to write: the window is no longer in column mode.
    Leave,
}

/// What one guarded write did.
enum Written {
    Nothing,
    Applied,
    Stale,
    Failed(String),
}

/// The options a write keeps with the layout.
fn option_commands(window: &str, count: usize, fit: Option<&str>) -> Result<Vec<String>, String> {
    let mut list = vec![format!("set -w -t {window} @masil-columns-count {count}")];
    match fit {
        Some("") => list.push(format!("set -wu -t {window} @masil-columns-fit")),
        Some(fit) => list.push(format!(
            "set -w -t {window} @masil-columns-fit {}",
            single_quoted(fit)?
        )),
        None => {}
    }
    list.push(format!(
        "set -wF -t {window} @masil-columns-prev '#{{window_layout}}'"
    ));
    list.push(format!(
        "set -wF -t {window} @masil-columns-place '{}'",
        place()
    ));
    list.push(format!("set -wu -t {window} @masil-columns-dirty"));
    Ok(list)
}

async fn write(server: &Server, state: &State, change: &Change) -> Result<Written, String> {
    let window = &state.window;
    // A focus change rewrites the layout's marks but not its geometry; prev
    // is read only for widths, so it stays current.
    let prev_current = || {
        state.prev == state.text
            || Layout::parse_prev(&state.prev)
                .is_ok_and(|prev| prev.geometry() == state.layout.geometry())
    };
    let options_current =
        |count: usize| state.count == Some(count) && !state.dirty && prev_current();
    let list = match change {
        Change::Layout { built, fit, extra } => {
            if !built.changed
                && extra.is_empty()
                && options_current(built.count)
                && &state.fit == fit
            {
                return Ok(Written::Nothing);
            }
            let mut list = Vec::new();
            if built.changed {
                list.push(format!(
                    "resize-window -t {window} -x {} -y {}",
                    built.width, built.height
                ));
                list.push(format!(
                    "select-layout -t {window} {}",
                    single_quoted(&built.json)?
                ));
            }
            list.extend(extra.iter().cloned());
            list.extend(option_commands(window, built.count, Some(fit))?);
            list
        }
        Change::Skip { count, .. } => {
            if options_current(*count) {
                return Ok(Written::Nothing);
            }
            option_commands(window, *count, None)?
        }
        Change::Leave => return Ok(Written::Nothing),
    };
    match server.run(&guarded(window, &state.signature, &list)).await {
        Ok(output) if output.trim() == "columns_applied" => Ok(Written::Applied),
        Ok(output) if output.trim() == "columns_stale" => Ok(Written::Stale),
        Ok(output) => Err(format!("columns: unexpected answer {}", output.trim())),
        Err(error) if error.starts_with("server_unreachable") => Err(error),
        Err(error) => Ok(Written::Failed(error)),
    }
}

/// Whether the window now has the built columns, the active pane read
/// before, and no last mark on it.
async fn check(server: &Server, state: &State, built: &Built) -> Result<bool, String> {
    let text = server
        .run(&[
            "list-panes",
            "-t",
            &state.window,
            "-F",
            "#{pane_id} #{pane_left} #{pane_active} #{pane_last}",
        ])
        .await?;
    let expected = Layout::parse(&built.json)?;
    let mut lefts = HashMap::new();
    for column in &expected.columns {
        for leaf in &column.leaves {
            lefts.insert(leaf.id.as_str(), leaf.x);
        }
    }
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [pane, left, active, last] = fields.as_slice() else {
            return Ok(false);
        };
        if let Some(x) = lefts.get(pane)
            && left.parse::<u32>().ok() != Some(*x)
        {
            return Ok(false);
        }
        if *active == "1" && (state.layout.active.as_deref() != Some(*pane) || *last == "1") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What `settle` did to a window.
#[derive(Debug, Default)]
struct Settled {
    changed: bool,
    widths: Vec<u32>,
    warnings: Vec<String>,
}

/// Reads a window, asks `make` for the change, and writes it in a guard:
/// a write that finds the window changed since the read reads again, up to
/// `STALE_ATTEMPTS` times; a layout that is not what was built is built and
/// written once more.
async fn settle<F>(server: &Server, window: &str, mut make: F) -> Result<Settled, String>
where
    F: FnMut(&State, &[Client], &mut Vec<String>) -> Result<Change, String>,
{
    settle_loop(server, window, false, |state, clients, warnings, _| {
        make(state, clients, warnings)
    })
    .await
}

/// `settle` for a change relative to the window (`width +N`, the `fit`
/// toggle). `make` learns whether a write of its own already applied: before
/// that it works the change out again from each read, which has any other
/// writer's change; after it, the read has its own change too, so it keeps
/// what it decided. A stale write after its own write applied stops: the
/// guard let another write in only on a read that had ours, and writing
/// again would undo it. A check that fails because another writer got in
/// first still writes again over that writer.
async fn settle_with<F>(server: &Server, window: &str, make: F) -> Result<Settled, String>
where
    F: FnMut(&State, &[Client], &mut Vec<String>, bool) -> Result<Change, String>,
{
    settle_loop(server, window, true, make).await
}

async fn settle_loop<F>(
    server: &Server,
    window: &str,
    relative: bool,
    mut make: F,
) -> Result<Settled, String>
where
    F: FnMut(&State, &[Client], &mut Vec<String>, bool) -> Result<Change, String>,
{
    let mut checked_once = false;
    let mut stale = 0;
    loop {
        let state = State::read(server, Some(window)).await?;
        let clients = server.clients().await?;
        let mut warnings = Vec::new();
        let change = make(&state, &clients, &mut warnings, checked_once)?;
        match write(server, &state, &change).await? {
            Written::Nothing => {
                return Ok(Settled {
                    changed: false,
                    widths: state.layout.widths(),
                    warnings,
                });
            }
            Written::Stale if relative && checked_once => {
                let state = State::read(server, Some(window)).await?;
                return Ok(Settled {
                    changed: true,
                    widths: state.layout.widths(),
                    warnings,
                });
            }
            Written::Stale => {
                stale += 1;
                if stale >= STALE_ATTEMPTS {
                    return Err(format!(
                        "columns_race: window {window} kept changing while it was laid out"
                    ));
                }
            }
            Written::Failed(error) => {
                // The list stopped at `select-layout`; `resize-window`
                // before it changed the panes, so the guard reads again.
                let state = State::read(server, Some(window)).await?;
                let count = state.layout.tiled();
                let list = option_commands(&state.window, count, None)?;
                server
                    .run(&guarded(&state.window, &state.signature, &list))
                    .await?;
                warnings.push(format!(
                    "columns_apply_failed: {}",
                    crate::managed::failure::human(&error)
                ));
                return Ok(Settled {
                    changed: true,
                    widths: state.layout.widths(),
                    warnings,
                });
            }
            Written::Applied => {
                let Change::Layout { built, .. } = &change else {
                    return Ok(Settled {
                        changed: false,
                        widths: state.layout.widths(),
                        warnings,
                    });
                };
                if !built.changed || check(server, &state, built).await? {
                    return Ok(Settled {
                        changed: built.changed,
                        widths: built.widths.clone(),
                        warnings,
                    });
                }
                if checked_once {
                    return Err(format!(
                        "columns_race: window {window} is not laid out as built"
                    ));
                }
                checked_once = true;
            }
        }
    }
}

/// The usual change: the window's columns with their kept widths, the
/// window as tall as its lowest viewing client. Builder refusals that a
/// later event can lift become a skip.
fn usual(
    state: &State,
    clients: &[Client],
    warnings: &mut Vec<String>,
    prev: Option<&Layout>,
    widths: HashMap<String, u32>,
    fit: BTreeMap<String, u32>,
    extra: Vec<String>,
) -> Result<Change, String> {
    skip_refused(
        state,
        warnings,
        asked(state, clients, prev, widths, fit, extra),
    )
}

/// The usual change for a command that asked for it: a builder refusal is
/// an error, since nothing the user asked for happened.
fn asked(
    state: &State,
    clients: &[Client],
    prev: Option<&Layout>,
    widths: HashMap<String, u32>,
    fit: BTreeMap<String, u32>,
    extra: Vec<String>,
) -> Result<Change, String> {
    let base = base_client(clients, &state.window, None);
    let request = Request {
        default_width: default_width(None, state, base),
        height: window_height(clients, &state.window),
        widths,
        place: None,
    };
    Ok(Change::Layout {
        built: build(&state.layout, prev, &request)?,
        fit: encode_fit(&fit),
        extra,
    })
}

/// A builder refusal that a later event can lift, as a skip with its
/// warning.
fn skip_refused(
    state: &State,
    warnings: &mut Vec<String>,
    change: Result<Change, String>,
) -> Result<Change, String> {
    match change {
        Err(error) if error.starts_with("columns_too_") => {
            warnings.push(error);
            Ok(Change::Skip {
                count: state.layout.tiled(),
            })
        }
        other => other,
    }
}

/// The hooks' slots: refuses when one holds a hook that is not ours.
async fn check_slots(server: &Server) -> Result<(), String> {
    let mut listing = server.run(&["show-hooks", "-g"]).await?;
    listing.push_str(&server.run(&["show-hooks", "-gw"]).await?);
    for (name, _) in hooks() {
        let slot = format!("{name}[{SLOT}] ");
        if let Some(line) = listing.lines().find(|line| line.starts_with(&slot))
            && !line.contains(MARK)
        {
            return Err(format!(
                "columns_hook_slot_taken:{name}: slot {SLOT} of {name} holds another hook"
            ));
        }
    }
    Ok(())
}

/// Hooks of a window's own or its session's that hide the global ones.
async fn shadowed(server: &Server, window: &str) -> Result<Vec<String>, String> {
    let mut listing = server.run(&["show-hooks", "-t", window]).await?;
    listing.push_str(&server.run(&["show-hooks", "-w", "-t", window]).await?);
    Ok(hooks()
        .into_iter()
        .filter(|(name, _)| {
            let prefix = format!("{name}[");
            listing.lines().any(|line| line.starts_with(&prefix))
        })
        .map(|(name, _)| format!("hook_shadowed:{name}"))
        .collect())
}

/// Writes the hooks, the monitor and the global options. Writing the same
/// hooks again changes nothing; the monitor and the options are written
/// only when they differ, so a monitor that is on does not fire again.
async fn install(server: &Server) -> Result<(), String> {
    check_slots(server).await?;
    let agent = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("columns: locating masil-agent: {error}"))?;
    let agent = agent
        .to_str()
        .ok_or("columns: the masil-agent path is not UTF-8")?
        .to_owned();
    let current = server
        .run(&[
            "display-message",
            "-p",
            "#{@masil-columns-agent}\t#{@masil-columns-offscreen}",
        ])
        .await?;
    let monitor_now = server.run(&["show-options", "-gqv", SYNC_MONITOR]).await?;
    let (current_agent, current_offscreen) = current
        .trim_end_matches('\n')
        .split_once('\t')
        .unwrap_or_default();
    let mut list: Vec<Vec<String>> = Vec::new();
    if current_agent != agent {
        list.push(vec![
            "set".into(),
            "-g".into(),
            "@masil-columns-agent".into(),
            agent,
        ]);
    }
    let offscreen = offscreen();
    if current_offscreen != offscreen {
        list.push(vec![
            "set".into(),
            "-g".into(),
            "@masil-columns-offscreen".into(),
            offscreen,
        ]);
    }
    for (name, hook) in hooks() {
        list.push(vec![
            "set-hook".into(),
            "-g".into(),
            format!("{name}[{SLOT}]"),
            hook,
        ]);
    }
    let (monitor, monitor_command) = sync_monitor();
    if monitor_now.trim_end_matches('\n') != monitor_command {
        list.push(vec![
            "set-hook".into(),
            "-g".into(),
            "-T".into(),
            "-B".into(),
            monitor,
            monitor_command.into(),
        ]);
    }
    server.run(&joined(list)).await?;
    Ok(())
}

/// Removes every hook of ours, the monitor and the global options. A slot
/// holding a hook that is not ours (the reason `on` refuses) keeps it.
async fn uninstall(server: &Server) -> Result<(), String> {
    let mut listing = server.run(&["show-hooks", "-g"]).await?;
    listing.push_str(&server.run(&["show-hooks", "-gw"]).await?);
    let mut list: Vec<Vec<String>> = removable(&listing)
        .into_iter()
        .map(|name| vec!["set-hook".into(), "-gu".into(), format!("{name}[{SLOT}]")])
        .collect();
    list.extend(
        GLOBALS
            .iter()
            .map(|option| vec!["set".into(), "-gu".into(), (*option).to_owned()]),
    );
    server.run(&joined(list)).await?;
    Ok(())
}

/// The hooks whose slot `uninstall` clears, by a `show-hooks` listing:
/// all of ours but a slot that holds another hook.
fn removable(listing: &str) -> Vec<&'static str> {
    hooks()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| {
            let slot = format!("{name}[{SLOT}] ");
            !listing
                .lines()
                .any(|line| line.starts_with(&slot) && !line.contains(MARK))
        })
        .collect()
}

/// Commands as one argument list, `;` between them.
fn joined(commands: Vec<Vec<String>>) -> Vec<String> {
    let mut args = Vec::new();
    for command in commands {
        if !args.is_empty() {
            args.push(";".to_owned());
        }
        args.extend(command);
    }
    args
}

async fn pan(server: &Server) -> Result<(), String> {
    server
        .run(&["run-shell".to_owned(), "-C".into(), pan_all()])
        .await
        .map(drop)
}

async fn on(server: &Server, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    let window = first.window.clone();
    check_slots(server).await?;
    let mut warnings = shadowed(server, &window).await?;
    if first.synchronize {
        warnings.push("synchronize_panes_on".into());
    }
    let fresh = !first.on;
    let mut own_size = None;
    if fresh {
        let own = server
            .run(&["show-options", "-wqv", "-t", &window, "window-size"])
            .await?;
        own_size = Some(own.trim().to_owned()).filter(|own| !own.is_empty());
        let saved = json!({
            "window_size": own_size,
            "layout": first.text,
        });
        server
            .run(&[
                "set",
                "-w",
                "-t",
                &window,
                "@masil-columns-saved",
                &saved.to_string(),
            ])
            .await?;
    }
    if first.zoomed {
        server.run(&["resize-pane", "-Z", "-t", &window]).await?;
    }
    let mark = format!("set -w -t {window} {MARK} 1");
    let result = settle(server, &window, |state, clients, warnings| {
        let base = base(clients, &state.window, options.client.as_deref())?;
        let request = Request {
            default_width: default_width(options.width, state, base),
            height: window_height(clients, &state.window),
            ..Request::default()
        };
        // A window already on keeps its widths.
        let prev = (!fresh).then(|| state.prev(warnings));
        let built = build(&state.layout, prev.as_ref(), &request)?;
        let mut extra = vec![mark.clone()];
        // A changed layout is written after a resize-window. One that already
        // fits gets it here: resize-window makes the window manual at this
        // size, so a smaller client cannot shrink it. Setting window-size
        // manual alone would snap the window to the size it was created at.
        if !built.changed {
            extra.push(format!(
                "resize-window -t {} -x {} -y {}",
                state.window, built.width, built.height
            ));
        }
        Ok(Change::Layout {
            built,
            fit: if fresh {
                String::new()
            } else {
                encode_fit(&state.fit(warnings))
            },
            extra,
        })
    })
    .await
    .and_then(|settled| {
        // The list stopped at select-layout, before the mark.
        match settled
            .warnings
            .iter()
            .find(|warning| warning.starts_with("columns_apply_failed"))
        {
            Some(failed) => Err(failed.clone()),
            None => Ok(settled),
        }
    });
    let settled = match result {
        Ok(settled) => settled,
        Err(error) => {
            if fresh {
                let _ = restore_size(server, &window, &own_size).await;
                let _ = clear_window(server, &window).await;
            }
            return Err(error);
        }
    };
    warnings.extend(settled.warnings);
    install(server).await?;
    pan(server).await?;
    Ok(json!({
        "stage": "columns_on",
        "window": window,
        "widths": settled.widths,
        "warnings": warnings,
    }))
}

async fn restore_size(server: &Server, window: &str, size: &Option<String>) -> Result<(), String> {
    match size {
        Some(size) => {
            server
                .run(&["set", "-w", "-t", window, "window-size", size])
                .await
        }
        None => {
            server
                .run(&["set", "-wu", "-t", window, "window-size"])
                .await
        }
    }
    .map(drop)
}

async fn clear_window(server: &Server, window: &str) -> Result<(), String> {
    let list = WINDOW_OPTIONS
        .iter()
        .map(|option| {
            vec![
                "set".into(),
                "-wu".into(),
                "-t".into(),
                window.to_owned(),
                (*option).to_owned(),
            ]
        })
        .collect();
    server.run(&joined(list)).await.map(drop)
}

async fn off(server: &Server, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    let window = first.window.clone();
    if !first.on {
        return Ok(json!({"stage": "columns_off", "window": window, "changed": false}));
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut stale = 0;
    let (had_layout, restored) = loop {
        let state = State::read(server, Some(&window)).await?;
        let saved: Value = serde_json::from_str(&state.saved).unwrap_or_else(|_| {
            if !warnings
                .iter()
                .any(|warning| warning == "columns_saved_unreadable")
            {
                warnings.push("columns_saved_unreadable".into());
            }
            Value::Null
        });
        let layout = match saved["layout"].as_str() {
            Some(text) => off_layout(text, &state.layout)?,
            None => None,
        };
        let mut list = Vec::new();
        if let Some(layout) = &layout {
            list.push(format!(
                "select-layout -t {window} {}",
                single_quoted(layout)?
            ));
        }
        for option in WINDOW_OPTIONS {
            list.push(format!("set -wu -t {window} {option}"));
        }
        for pane in state.layout.panes() {
            list.push(format!("set -pu -t {pane} @masil-attention"));
        }
        list.push(match saved["window_size"].as_str() {
            Some(size) => format!("set -w -t {window} window-size {}", single_quoted(size)?),
            None => format!("set -wu -t {window} window-size"),
        });
        match server
            .run(&guarded(&window, &state.signature, &list))
            .await?
            .trim()
        {
            "columns_applied" => break (saved["layout"].is_string(), layout.is_some()),
            "columns_stale" => {
                stale += 1;
                if stale >= STALE_ATTEMPTS {
                    return Err(format!(
                        "columns_race: window {window} kept changing while column mode was turned off"
                    ));
                }
            }
            other => return Err(format!("columns: unexpected answer {other}")),
        }
    };
    if had_layout && !restored {
        warnings
            .push("columns_layout_kept: the panes changed since column mode was turned on".into());
    }
    if server.column_windows().await?.is_empty() {
        uninstall(server).await?;
    }
    Ok(json!({
        "stage": "columns_off",
        "window": window,
        "changed": true,
        "warnings": warnings,
    }))
}

/// The layout `off` puts back: the saved one's tiled part when the tiled
/// panes are the same, with the current floating cells, every cell's `"i"`
/// numbered by the pane's current place in the list and the current
/// last-pane marks. None when the tiled panes differ.
fn off_layout(saved: &str, current: &Layout) -> Result<Option<String>, String> {
    let Ok(parsed) = Layout::parse(saved) else {
        return Ok(None);
    };
    if current.columns.is_empty() {
        return Ok(None);
    }
    let ids = |layout: &Layout| {
        let mut ids: Vec<String> = layout.column_panes().into_iter().flatten().collect();
        ids.sort();
        ids
    };
    if ids(&parsed) != ids(current) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(saved).map_err(|error| error.to_string())?;
    let Some(mut root) = strip_floating(&value["L"]) else {
        return Ok(None);
    };
    // The current place and last mark of every pane, tiled and floating.
    let mut places = HashMap::new();
    let mut lasts = HashMap::new();
    for column in &current.columns {
        for leaf in &column.leaves {
            places.insert(leaf.id.clone(), index_of(&leaf.value)?);
            if let Some(last) = leaf.value.get("l") {
                lasts.insert(leaf.id.clone(), last.clone());
            }
        }
    }
    for cell in &current.floating {
        floating_places(cell, &mut places, &mut lasts)?;
    }
    let mut sorted: Vec<u64> = places.values().copied().collect();
    sorted.sort_unstable();
    let rank: HashMap<u64, usize> = sorted
        .iter()
        .enumerate()
        .map(|(rank, place)| (*place, rank))
        .collect();
    let active = current.active.as_deref();
    renumber_saved(&mut root, &places, &rank, &lasts, active)?;
    let (width, height) = (number(&root, "w")?, number(&root, "h")?);
    let mut floating = Vec::new();
    for cell in &current.floating {
        let mut cell = cell.clone();
        let (x, y, w, h) = (
            number(&cell, "x").unwrap_or(0),
            number(&cell, "y").unwrap_or(0),
            number(&cell, "w").unwrap_or(0),
            number(&cell, "h").unwrap_or(0),
        );
        let dx = i64::from(pull_in(x, w, width)) - i64::from(x);
        let dy = i64::from(pull_in(y, h, height)) - i64::from(y);
        renumber_floating(&mut cell, &rank, active, dx, dy)?;
        floating.push(cell);
    }
    if !floating.is_empty() {
        if root["c"].is_array() {
            if let Some(children) = root["c"].as_array_mut() {
                children.extend(floating);
            }
        } else {
            let mut children = vec![root];
            children.extend(floating);
            root = json!({"t":"h","w":width,"h":height,"x":0,"y":0,"c":children});
        }
    }
    serde_json::to_string(&json!({"V": 2, "L": root}))
        .map(Some)
        .map_err(|error| error.to_string())
}

/// A saved layout without its floating cells; a container left with one
/// cell becomes that cell.
fn strip_floating(cell: &Value) -> Option<Value> {
    if cell.get("z").is_some() {
        return None;
    }
    let Some(children) = cell["c"].as_array() else {
        return Some(cell.clone());
    };
    let mut kept: Vec<Value> = children.iter().filter_map(strip_floating).collect();
    match kept.len() {
        0 => None,
        1 => kept.pop(),
        _ => {
            let mut cell = cell.clone();
            cell["c"] = Value::Array(kept);
            Some(cell)
        }
    }
}

fn floating_places(
    cell: &Value,
    places: &mut HashMap<String, u64>,
    lasts: &mut HashMap<String, Value>,
) -> Result<(), String> {
    match cell["c"].as_array() {
        Some(children) => {
            for child in children {
                floating_places(child, places, lasts)?;
            }
        }
        None => {
            if let Some(id) = cell["I"].as_str() {
                places.insert(id.to_owned(), index_of(cell)?);
                if let Some(last) = cell.get("l") {
                    lasts.insert(id.to_owned(), last.clone());
                }
            }
        }
    }
    Ok(())
}

fn renumber_saved(
    cell: &mut Value,
    places: &HashMap<String, u64>,
    rank: &HashMap<u64, usize>,
    lasts: &HashMap<String, Value>,
    active: Option<&str>,
) -> Result<(), String> {
    let object = cell.as_object_mut().ok_or("invalid layout: cell")?;
    object.remove("a");
    if let Some(children) = object.get_mut("c").and_then(Value::as_array_mut) {
        for child in children {
            renumber_saved(child, places, rank, lasts, active)?;
        }
        return Ok(());
    }
    let id = object
        .get("I")
        .and_then(Value::as_str)
        .ok_or("invalid layout: pane cell without a pane id")?
        .to_owned();
    let place = places
        .get(&id)
        .ok_or("invalid layout: a saved pane is gone")?;
    object.insert("i".into(), json!(rank[place]));
    object.remove("l");
    if active != Some(id.as_str())
        && let Some(last) = lasts.get(&id)
    {
        object.insert("l".into(), last.clone());
    }
    Ok(())
}

async fn add(server: &Server, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    first.require_on()?;
    first.require_tiled()?;
    let window = first.window.clone();
    let target = first.pane.clone();
    let clients = server.clients().await?;
    let width =
        default_width(options.width, &first, base_client(&clients, &window, None)).max(MIN_WIDTH);
    let wider = first.width + width + 1;
    if wider > MAX_WINDOW {
        return Err(format!(
            "columns_too_wide: the columns need {wider} cells, more than {MAX_WINDOW}"
        ));
    }
    let mut warnings = Vec::new();
    let prev = first.prev_before_change(&mut warnings);
    let mut split = vec![
        "split-window".to_owned(),
        "-h".into(),
        "-f".into(),
        "-l".into(),
        width.to_string(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
        "-t".into(),
        target.clone(),
    ];
    if options.detached {
        split.push("-d".into());
    }
    if let Some(cwd) = &options.cwd {
        split.extend(["-c".into(), cwd.clone()]);
    }
    append_command(&mut split, options.command.as_deref());
    // Wider first, so the split has room; then a column at the right end.
    // The dirty mark keeps the layout hook from saving the widened layout
    // as prev; the write below, or the helper the split starts, clears it.
    let list = joined(vec![
        [
            "set",
            "-w",
            "-t",
            window.as_str(),
            "@masil-columns-dirty",
            "1",
        ]
        .map(str::to_owned)
        .to_vec(),
        vec![
            "resize-window".into(),
            "-t".into(),
            window.clone(),
            "-x".into(),
            wider.to_string(),
        ],
        split,
    ]);
    let created = match server.run(&list).await {
        Ok(output) => output.trim().to_owned(),
        Err(error) => {
            let mut undo = vec![vec![
                "resize-window".to_owned(),
                "-t".into(),
                window.clone(),
                "-x".into(),
                first.width.to_string(),
            ]];
            if !first.dirty {
                undo.push(vec![
                    "set".into(),
                    "-wu".into(),
                    "-t".into(),
                    window.clone(),
                    "@masil-columns-dirty".into(),
                ]);
            }
            let _ = server.run(&joined(undo)).await;
            return Err(error);
        }
    };
    if crate::pane_id(&created).is_err() {
        return Err("columns: split-window returned no pane id".into());
    }
    let settled = settle(server, &window, |state, clients, warnings| {
        let base = base_client(clients, &state.window, None);
        let mut request = Request {
            default_width: default_width(None, state, base),
            height: window_height(clients, &state.window),
            ..Request::default()
        };
        request.widths.insert(created.clone(), width);
        if state.layout.column_of(&created).is_some() && state.layout.column_of(&target).is_some() {
            request.place = Some((created.clone(), target.clone(), options.before));
        }
        // The pane is made: a refusal leaves it where tmux put it, with a
        // warning, as for any other event.
        let fit = encode_fit(&state.fit(warnings));
        let change = build(&state.layout, Some(&prev), &request).map(|built| Change::Layout {
            built,
            fit,
            extra: Vec::new(),
        });
        skip_refused(state, warnings, change)
    })
    .await?;
    warnings.extend(settled.warnings);
    pan(server).await?;
    Ok(json!({
        "stage": "column_added",
        "window": window,
        "pane": created,
        "widths": settled.widths,
        "warnings": warnings,
    }))
}

/// A command for `split-window` or `new-pane`, as `layout` passes one: a
/// single word runs without a shell.
fn append_command(args: &mut Vec<String>, command: Option<&[String]>) {
    let Some(command) = command else {
        return;
    };
    args.push("--".into());
    if command.len() == 1 {
        args.extend(["/usr/bin/env".into(), "--".into(), command[0].clone()]);
    } else {
        args.extend(command.iter().cloned());
    }
}

async fn close(server: &Server, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    first.require_on()?;
    let window = first.window.clone();
    let pane = first.pane.clone();
    if first.pane_agent && !options.force {
        return Err(format!(
            "pane_has_agent: {pane} runs a managed agent; use agent stop NAME, or --force"
        ));
    }
    let mut warnings = Vec::new();
    let prev = first.prev_before_change(&mut warnings);
    let alone = first.layout.panes().len() == 1;
    server.run(&["kill-pane", "-t", &pane]).await?;
    if alone {
        return Ok(
            json!({"stage": "column_closed", "window": window, "pane": pane, "window_closed": true}),
        );
    }
    let settled = settle(server, &window, |state, clients, warnings| {
        let fit = state.fit(warnings);
        usual(
            state,
            clients,
            warnings,
            Some(&prev),
            HashMap::new(),
            fit,
            Vec::new(),
        )
    })
    .await?;
    warnings.extend(settled.warnings);
    pan(server).await?;
    Ok(json!({
        "stage": "column_closed",
        "window": window,
        "pane": pane,
        "widths": settled.widths,
        "warnings": warnings,
    }))
}

/// The width a column has now, as the builder keeps it.
fn kept_width(state: &State, prev: &Layout, pane: &str, clients: &[Client]) -> Result<u32, String> {
    let column = state
        .layout
        .column_of(pane)
        .ok_or_else(|| format!("invalid_target: {pane} is not tiled"))?;
    let base = base_client(clients, &state.window, None);
    let request = Request {
        default_width: default_width(None, state, base),
        ..Request::default()
    };
    Ok(build(&state.layout, Some(prev), &request)?.widths[column])
}

async fn width(server: &Server, change: WidthChange, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    first.require_on()?;
    first.require_tiled()?;
    let window = first.window.clone();
    let pane = first.pane.clone();
    // Kept once a write of ours applied: the next read has that write, and
    // `+N` must not apply twice.
    let mut decided: Option<u32> = None;
    let settled = settle_with(server, &window, |state, clients, warnings, applied| {
        let prev = state.prev(warnings);
        let next = match decided.filter(|_| applied) {
            Some(next) => next,
            None => {
                let current = kept_width(state, &prev, &pane, clients)?;
                let view = base(clients, &state.window, options.client.as_deref())?
                    .map(|client| client.width);
                let next = change
                    .apply(current, view)
                    .ok_or_else(|| {
                        format!(
                            "client_ambiguous: no client views {}; pass --client NAME",
                            state.window
                        )
                    })?
                    .max(MIN_WIDTH);
                *decided.insert(next)
            }
        };
        let column = state
            .layout
            .column_of(&pane)
            .ok_or_else(|| format!("invalid_target: {pane} is not tiled"))?;
        let panes = &state.layout.column_panes()[column];
        let mut fit = state.fit(warnings);
        fit.retain(|key, _| !panes.contains(key));
        let widths = HashMap::from([(pane.clone(), next)]);
        asked(state, clients, Some(&prev), widths, fit, Vec::new())
    })
    .await?;
    pan(server).await?;
    Ok(json!({
        "stage": "column_width",
        "window": window,
        "pane": pane,
        "widths": settled.widths,
        "warnings": settled.warnings,
    }))
}

async fn fit(server: &Server, options: &Options) -> Result<Value, String> {
    let first = State::read(server, options.target.as_deref()).await?;
    first.require_on()?;
    first.require_tiled()?;
    let window = first.window.clone();
    let pane = first.pane.clone();
    // Kept as in `width`: whether the column is fitted now, its width, and
    // the width its fit entry keeps for later.
    let mut decided: Option<(bool, u32, u32)> = None;
    let settled = settle_with(server, &window, |state, clients, warnings, applied| {
        // A named client must exist even when nothing needs its width.
        base(clients, &state.window, options.client.as_deref())?;
        let prev = state.prev(warnings);
        let column = state
            .layout
            .column_of(&pane)
            .ok_or_else(|| format!("invalid_target: {pane} is not tiled"))?;
        let panes = state.layout.column_panes()[column].clone();
        let mut fit = state.fit(warnings);
        let (fitted, width, kept) = match decided.filter(|_| applied) {
            Some(decision) => decision,
            None => {
                let current = kept_width(state, &prev, &pane, clients)?;
                let decision = match panes.iter().find_map(|key| fit.get(key)) {
                    Some(kept) => (false, *kept, 0),
                    None => {
                        let base = require_base(clients, &state.window, options.client.as_deref())?;
                        (true, base.width, current)
                    }
                };
                *decided.insert(decision)
            }
        };
        fit.retain(|key, _| !panes.contains(key));
        if fitted {
            fit.insert(pane.clone(), kept);
        }
        let widths = HashMap::from([(pane.clone(), width.max(MIN_WIDTH))]);
        asked(state, clients, Some(&prev), widths, fit, Vec::new())
    })
    .await?;
    let fitted = decided.is_some_and(|(fitted, ..)| fitted);
    pan(server).await?;
    Ok(json!({
        "stage": if fitted { "column_fitted" } else { "column_unfitted" },
        "window": window,
        "pane": pane,
        "widths": settled.widths,
        "warnings": settled.warnings,
    }))
}

async fn float(server: &Server, options: &Options) -> Result<Value, String> {
    let state = State::read(server, options.target.as_deref()).await?;
    state.require_on()?;
    let clients = server.clients().await?;
    let client = require_base(&clients, &state.window, options.client.as_deref())?;
    // `new-pane -x/-y` sizes take the border; `-X/-Y` place it.
    let width = options
        .size
        .0
        .unwrap_or((client.width / 2).max(20))
        .clamp(3, client.width.max(3));
    let height = options
        .size
        .1
        .unwrap_or((client.height / 2).max(5))
        .clamp(3, client.height.max(3));
    let x = client.x + client.width.saturating_sub(width) / 2;
    let y = client.y + client.height.saturating_sub(height) / 2;
    let mut args = vec![
        "new-pane".to_owned(),
        "-t".into(),
        state.window.clone(),
        "-X".into(),
        x.to_string(),
        "-Y".into(),
        y.to_string(),
        "-x".into(),
        width.to_string(),
        "-y".into(),
        height.to_string(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
    ];
    if let Some(cwd) = &options.cwd {
        args.extend(["-c".into(), cwd.clone()]);
    }
    append_command(&mut args, options.command.as_deref());
    let pane = server.run(&args).await?.trim().to_owned();
    Ok(json!({
        "stage": "float_created",
        "window": state.window,
        "pane": pane,
        "client": client.name,
    }))
}

async fn sync(server: &Server, options: &Options) -> Result<Value, String> {
    server
        .run(&["set", "-gu", "@masil-columns-pending"])
        .await?;
    let mut windows = server.column_windows().await?;
    if let Some(target) = &options.window {
        let id = server
            .run(&["display-message", "-p", "-t", target, "#{window_id}"])
            .await?;
        windows.retain(|window| window == id.trim());
    }
    if windows.is_empty() {
        return Ok(json!({"stage": "columns_synced", "windows": []}));
    }
    install(server).await?;
    // An `off` between the list above and the install found no column
    // window and uninstalled first.
    if server.column_windows().await?.is_empty() {
        uninstall(server).await?;
        return Ok(json!({"stage": "columns_synced", "windows": []}));
    }
    let mut report = Vec::new();
    for window in windows {
        let result = settle(server, &window, |state, clients, warnings| {
            if !state.on {
                warnings.push("columns_off".into());
                return Ok(Change::Leave);
            }
            let prev = state.prev(warnings);
            let fit = state.fit(warnings);
            usual(
                state,
                clients,
                warnings,
                Some(&prev),
                HashMap::new(),
                fit,
                Vec::new(),
            )
        })
        .await;
        report.push(match result {
            Ok(settled) => json!({
                "window": window,
                "changed": settled.changed,
                "widths": settled.widths,
                "warnings": settled.warnings,
            }),
            Err(error) => json!({"window": window, "error": error}),
        });
    }
    pan(server).await?;
    Ok(json!({"stage": "columns_synced", "windows": report}))
}

async fn show(server: &Server, options: &Options) -> Result<Value, String> {
    let state = State::read(server, options.target.as_deref()).await?;
    let clients = server.clients().await?;
    let base = base_client(&clients, &state.window, None);
    let size = server
        .run(&["show-options", "-wqv", "-t", &state.window, "window-size"])
        .await?;
    let mut listing = server.run(&["show-hooks", "-g"]).await?;
    listing.push_str(&server.run(&["show-hooks", "-gw"]).await?);
    let slot = format!("[{SLOT}] ");
    let installed: Vec<&str> = listing
        .lines()
        .filter(|line| line.contains(&slot) && line.contains(MARK))
        .filter_map(|line| line.split_once('[').map(|(name, _)| name))
        .collect();
    let globals = server
        .run(&[
            "display-message",
            "-p",
            "#{@masil-columns-pending}\t#{@masil-columns-agent}",
        ])
        .await?;
    let (pending, agent) = globals
        .trim_end_matches('\n')
        .split_once('\t')
        .unwrap_or_default();
    let monitor = server.run(&["show-options", "-gqv", SYNC_MONITOR]).await?;
    let mut warnings = Vec::new();
    let preview = if state.on {
        let prev = state.prev(&mut warnings);
        let fit = state.fit(&mut warnings);
        match usual(
            &state,
            &clients,
            &mut warnings,
            Some(&prev),
            HashMap::new(),
            fit,
            Vec::new(),
        )? {
            Change::Layout { built, .. } => {
                json!({"changed": built.changed, "widths": built.widths})
            }
            Change::Skip { .. } | Change::Leave => json!({"changed": false}),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "window": state.window,
        "on": state.on,
        "widths": state.layout.widths(),
        "columns": state.layout.column_panes(),
        "size": state.layout.size(),
        "window_size": size.trim(),
        "hooks": installed,
        "monitor": !monitor.trim().is_empty(),
        "pending": !pending.is_empty(),
        "agent": agent,
        "base_client": base.map(|client| client.name.clone()),
        "next": preview,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_keeps_a_hook_that_is_not_ours() {
        let all: Vec<&str> = hooks().into_iter().map(|(name, _)| name).collect();
        assert_eq!(removable(""), all);
        let listing = format!(
            "client-attached[4141] display-message foreign\nwindow-zoomed[4141] if -F \"#{{{MARK}}}\" {{}}\nclient-attached[7] x\n"
        );
        let kept: Vec<&str> = all
            .iter()
            .copied()
            .filter(|name| *name != "client-attached")
            .collect();
        assert_eq!(removable(&listing), kept);
    }

    #[test]
    fn a_sync_entry_fails_on_an_error_or_an_unapplied_layout() {
        assert_eq!(
            sync_failure(&json!({"window": "@1", "error": "columns_race: x"})),
            Some("columns_race: x".into())
        );
        assert_eq!(
            sync_failure(&json!({
                "window": "@1",
                "changed": true,
                "warnings": ["prev_unreadable", "columns_apply_failed: no space"],
            })),
            Some("columns_apply_failed: no space".into())
        );
        assert_eq!(
            sync_failure(&Value::Null),
            Some("columns: the window is not in column mode".into())
        );
        assert_eq!(
            sync_failure(&json!({
                "window": "@1",
                "changed": false,
                "widths": [60, 60],
                "warnings": ["hook_shadowed:after-split-window"],
            })),
            None
        );
    }

    /// A layout of columns (each a list of `(pane, height)`) with the given
    /// widths, in pane-list order left to right, and floating cells.
    fn layout(columns: &[(u32, &[(&str, u32)])], height: u32, floating: &[Value]) -> String {
        let mut cells = Vec::new();
        let mut x = 0;
        let mut index = 0;
        for (width, panes) in columns {
            let mut leaves = Vec::new();
            let mut y = 0;
            for (id, h) in *panes {
                leaves.push(json!({"t":"p","w":width,"h":h,"x":x,"y":y,"i":index,"I":id}));
                index += 1;
                y += h + 1;
            }
            cells.push(if leaves.len() == 1 {
                leaves.pop().unwrap()
            } else {
                json!({"t":"v","w":width,"h":height,"x":x,"y":0,"c":leaves})
            });
            x += width + 1;
        }
        cells.extend(floating.iter().cloned());
        json!({"V":2,"L":{"t":"h","w":x-1,"h":height,"x":0,"y":0,"c":cells}}).to_string()
    }

    fn parse(text: &str) -> Layout {
        Layout::parse(text).unwrap()
    }

    fn request(default_width: u32) -> Request {
        Request {
            default_width,
            ..Request::default()
        }
    }

    #[test]
    fn columns_are_grouped_by_left_edge_and_stacked_by_top() {
        let text = layout(
            &[(50, &[("%1", 40)]), (60, &[("%2", 20), ("%3", 19)])],
            40,
            &[],
        );
        let parsed = parse(&text);
        assert_eq!(parsed.widths(), [50, 60]);
        assert_eq!(parsed.column_panes(), [vec!["%1"], vec!["%2", "%3"]]);
        assert_eq!(parsed.tiled(), 3);
        assert_eq!(parsed.column_of("%3"), Some(1));
    }

    #[test]
    fn on_gives_every_column_the_default_width() {
        let current = parse(&layout(
            &[(100, &[("%1", 40)]), (99, &[("%2", 40)])],
            40,
            &[],
        ));
        let built = build(&current, None, &request(80)).unwrap();
        assert_eq!(built.widths, [80, 80]);
        assert_eq!(built.width, 161);
        assert!(built.changed);
        assert_eq!(parse(&built.json).widths(), [80, 80]);
    }

    #[test]
    fn a_split_column_gets_the_default_and_its_neighbour_its_old_width() {
        // %2 was dragged to 120 before `split-window -h -t %2` made %4.
        let prev = parse(&layout(
            &[
                (80, &[("%1", 40)]),
                (120, &[("%2", 40)]),
                (40, &[("%3", 40)]),
            ],
            40,
            &[],
        ));
        let current = parse(&layout(
            &[
                (80, &[("%1", 40)]),
                (60, &[("%2", 40)]),
                (59, &[("%4", 40)]),
                (40, &[("%3", 40)]),
            ],
            40,
            &[],
        ));
        let built = build(&current, Some(&prev), &request(70)).unwrap();
        assert_eq!(built.widths, [80, 120, 70, 40]);
    }

    #[test]
    fn a_killed_column_gives_its_neighbour_back_its_width() {
        let prev = parse(&layout(
            &[
                (50, &[("%1", 40)]),
                (80, &[("%2", 40)]),
                (30, &[("%3", 40)]),
            ],
            40,
            &[],
        ));
        let current = parse(&layout(
            &[(131, &[("%1", 40)]), (30, &[("%3", 40)])],
            40,
            &[],
        ));
        let built = build(&current, Some(&prev), &request(70)).unwrap();
        assert_eq!(built.widths, [50, 30]);
    }

    #[test]
    fn add_puts_the_new_column_beside_its_target_and_keeps_old_widths() {
        // `resize-window` and the split moved the old widths; `prev` is the
        // read before the split.
        let prev = parse(&layout(
            &[
                (50, &[("%1", 40)]),
                (80, &[("%2", 40)]),
                (30, &[("%3", 40)]),
            ],
            40,
            &[],
        ));
        let current = parse(&layout(
            &[
                (51, &[("%1", 40)]),
                (72, &[("%2", 40)]),
                (37, &[("%3", 40)]),
                (60, &[("%4", 40)]),
            ],
            40,
            &[],
        ));
        let mut ask = request(60);
        ask.place = Some(("%4".into(), "%1".into(), false));
        let built = build(&current, Some(&prev), &ask).unwrap();
        assert_eq!(built.widths, [50, 60, 80, 30]);
        assert_eq!(
            parse(&built.json).column_panes(),
            [vec!["%1"], vec!["%4"], vec!["%2"], vec!["%3"]]
        );
        // Before the target.
        ask.place = Some(("%4".into(), "%1".into(), true));
        let built = build(&current, Some(&prev), &ask).unwrap();
        assert_eq!(parse(&built.json).column_panes()[0], vec!["%4".to_owned()]);
    }

    #[test]
    fn joined_and_parted_columns_keep_one_old_width() {
        let prev = parse(&layout(
            &[(50, &[("%1", 40)]), (80, &[("%2", 40)])],
            40,
            &[],
        ));
        // %2 joined below %1.
        let joined = parse(&layout(&[(131, &[("%1", 20), ("%2", 19)])], 40, &[]));
        assert_eq!(
            build(&joined, Some(&prev), &request(70)).unwrap().widths,
            [50]
        );
        // %2 moved out of a stack into its own column.
        let prev = parse(&layout(&[(50, &[("%1", 20), ("%2", 19)])], 40, &[]));
        let parted = parse(&layout(
            &[(25, &[("%1", 40)]), (24, &[("%2", 40)])],
            40,
            &[],
        ));
        assert_eq!(
            build(&parted, Some(&prev), &request(70)).unwrap().widths,
            [50, 70]
        );
    }

    #[test]
    fn an_unchanged_window_needs_no_layout_write() {
        let text = layout(
            &[(50, &[("%1", 40)]), (80, &[("%2", 20), ("%3", 19)])],
            40,
            &[],
        );
        let current = parse(&text);
        let built = build(&current, Some(&current), &request(70)).unwrap();
        assert!(!built.changed);
        assert_eq!(built.count, 3);
    }

    #[test]
    fn stacked_heights_follow_the_window_and_keep_one_row_each() {
        let current = parse(&layout(&[(50, &[("%1", 30), ("%2", 9)])], 40, &[]));
        let mut ask = request(70);
        ask.height = Some(20);
        let built = build(&current, Some(&current), &ask).unwrap();
        let heights: Vec<u32> = parse(&built.json).columns[0]
            .leaves
            .iter()
            .map(|leaf| leaf.height)
            .collect();
        assert_eq!(heights.iter().sum::<u32>() + 1, 20);
        assert!(heights.iter().all(|h| *h >= 1));
        // Three panes in four rows: one row each and two borders is five.
        let tall = parse(&layout(
            &[(50, &[("%1", 13), ("%2", 13), ("%3", 12)])],
            40,
            &[],
        ));
        ask.height = Some(4);
        let error = build(&tall, Some(&tall), &ask).unwrap_err();
        assert!(error.starts_with("columns_too_short"), "{error}");
        ask.height = Some(5);
        assert!(build(&tall, Some(&tall), &ask).is_ok());
    }

    #[test]
    fn a_window_with_only_floating_panes_has_no_columns_and_is_refused() {
        // The core writes such a window's root as the floating cell itself.
        let float = json!({"t":"p","w":28,"h":6,"x":3,"y":2,"i":0,"z":0,"a":true,"I":"%9"});
        let text = json!({"V":2,"L":float}).to_string();
        let current = parse(&text);
        assert!(current.widths().is_empty());
        assert_eq!(current.tiled(), 0);
        assert_eq!(current.panes(), vec!["%9".to_owned()]);
        let error = build(&current, None, &request(60)).unwrap_err();
        assert!(error.starts_with("columns_too_few"), "{error}");
        let saved = layout(&[(60, &[("%1", 24)])], 24, &[]);
        assert_eq!(off_layout(&saved, &current), Ok(None));
    }

    #[test]
    fn floating_cells_move_to_the_root_and_inside_the_window() {
        let float = json!({"t":"p","w":28,"h":6,"x":300,"y":2,"i":2,"z":0,"a":true,"l":0,"I":"%9"});
        let text = layout(&[(100, &[("%1", 40)]), (99, &[("%2", 40)])], 40, &[float]);
        let current = parse(&text);
        assert_eq!((current.tiled(), current.floating.len()), (2, 1));
        let built = build(&current, None, &request(50)).unwrap();
        let value: Value = serde_json::from_str(&built.json).unwrap();
        let cells = value["L"]["c"].as_array().unwrap();
        let moved = cells.iter().find(|cell| cell["I"] == "%9").unwrap();
        assert!(moved["x"].as_u64().unwrap() + 28 < 101, "{moved}");
        // The active pane loses "a" and its own "l".
        assert!(moved.get("a").is_none() && moved.get("l").is_none());
    }

    #[test]
    fn indexes_keep_the_pane_list_order_from_zero() {
        // A dump with pane-base-index 1.
        let text = r#"{"V":2,"L":{"t":"h","w":200,"h":40,"x":0,"y":0,"c":[
            {"t":"p","w":100,"h":40,"x":0,"y":0,"i":1,"I":"%5"},
            {"t":"v","w":99,"h":40,"x":101,"y":0,"c":[
                {"t":"p","w":99,"h":20,"x":101,"y":0,"i":2,"l":0,"I":"%6"},
                {"t":"p","w":99,"h":19,"x":101,"y":21,"i":3,"a":true,"I":"%7"},
                {"t":"p","w":20,"h":5,"x":4,"y":2,"i":4,"z":0,"I":"%8"}]}]}}"#;
        let current = parse(text);
        let built = build(&current, None, &request(60)).unwrap();
        let next: Value = serde_json::from_str(&built.json).unwrap();
        let mut found = Vec::new();
        fn collect(cell: &Value, found: &mut Vec<(String, u64, Option<u64>)>) {
            match cell["c"].as_array() {
                Some(children) => children.iter().for_each(|child| collect(child, found)),
                None => found.push((
                    cell["I"].as_str().unwrap().into(),
                    cell["i"].as_u64().unwrap(),
                    cell["l"].as_u64(),
                )),
            }
        }
        collect(&next["L"], &mut found);
        found.sort();
        // The last-pane mark of another pane stays; nothing is active.
        assert_eq!(
            found,
            [
                ("%5".into(), 0, None),
                ("%6".into(), 1, Some(0)),
                ("%7".into(), 2, None),
                ("%8".into(), 3, None)
            ]
        );
        assert!(!built.json.contains(r#""a":true"#));
    }

    #[test]
    fn explicit_widths_win_and_widths_are_bounded() {
        let current = parse(&layout(
            &[(50, &[("%1", 40)]), (80, &[("%2", 40)])],
            40,
            &[],
        ));
        let mut ask = request(70);
        ask.widths.insert("%2".into(), 5);
        assert_eq!(
            build(&current, Some(&current), &ask).unwrap().widths,
            [50, MIN_WIDTH]
        );
        ask.widths.insert("%2".into(), MAX_WINDOW);
        let error = build(&current, Some(&current), &ask).unwrap_err();
        assert!(error.starts_with("columns_too_wide"), "{error}");
    }

    #[test]
    fn broken_layouts_are_refused() {
        assert!(Layout::parse("not json").is_err());
        assert!(Layout::parse(r#"{"V":1,"L":{}}"#).is_err());
        assert!(Layout::parse(r#"{"V":2,"L":{"t":"q","w":1,"h":1,"x":0,"y":0}}"#).is_err());
        assert!(Layout::parse(r#"{"V":2,"L":{"t":"p","w":1,"h":1,"x":0,"y":0,"i":0}}"#).is_err());
    }

    #[test]
    fn a_client_area_leaves_out_its_status_line_or_column() {
        assert_eq!(area(200, 50, "on", "bottom", ""), (200, 49));
        assert_eq!(area(200, 50, "3", "top", ""), (200, 47));
        assert_eq!(area(200, 50, "off", "bottom", ""), (200, 50));
        assert_eq!(area(200, 50, "on", "left", ""), (176, 50));
        assert_eq!(area(200, 50, "on", "right", "30"), (170, 50));
        assert_eq!(area(200, 50, "on", "right", "200"), (176, 50));
        // Too narrow for the column, too short for the line.
        assert_eq!(area(30, 50, "on", "left", ""), (30, 50));
        assert_eq!(area(200, 1, "on", "bottom", ""), (200, 1));
    }

    #[test]
    fn a_window_is_sized_for_its_narrowest_and_lowest_counted_client() {
        let text = "a\t0\tattached,focused\t@1\t200\t50\ton\tbottom\t\t0\t0\n\
                    b\t0\tattached,ignore-size\t@1\t80\t20\ton\tbottom\t\t0\t0\n\
                    c\t1\tattached,control-mode\t@1\t60\t10\toff\tbottom\t\t0\t0\n\
                    d\t0\tattached\t@1\t120\t60\ton\tbottom\t\t40\t0\n\
                    e\t0\tattached\t@2\t40\t10\ton\tbottom\t\t0\t0\n\
                    broken line";
        let clients = parse_clients(text);
        assert_eq!(clients.len(), 5);
        assert_eq!(base_client(&clients, "@1", None).unwrap().name, "d");
        assert_eq!(base_client(&clients, "@1", None).unwrap().x, 40);
        assert_eq!(window_height(&clients, "@1"), Some(49));
        assert_eq!(base_client(&clients, "@1", Some("b")).unwrap().name, "b");
        assert!(base_client(&clients, "@3", None).is_none());
        assert_eq!(window_height(&clients, "@3"), None);
    }

    #[test]
    fn every_hook_is_marked_and_starts_processes_quietly() {
        let hooks = hooks();
        assert_eq!(hooks.len(), 9);
        for (name, command) in &hooks {
            assert!(command.contains(MARK), "{name}");
            assert!(!command.contains("run-shell -b"), "{name}");
            assert_eq!(
                command.matches('{').count(),
                command.matches('}').count(),
                "{name}"
            );
        }
        let closed = &hooks
            .iter()
            .find(|(name, _)| *name == "window-closed")
            .unwrap()
            .1;
        for (name, _) in &hooks {
            assert!(
                closed.contains(&format!("set-hook -gu {name}[{SLOT}]")),
                "{name}"
            );
        }
        assert!(closed.contains("set -gu @masil-columns-sync"));
        let (monitor, _) = sync_monitor();
        assert!(monitor.starts_with("@masil-columns-sync::#{S:#{W:"));
    }

    #[test]
    fn an_apply_runs_only_where_the_panes_were_read() {
        let command = guarded(
            "@3",
            "%1.0.0.80.40:",
            &[
                "select-layout -t @3 '{}'".into(),
                "set -w -t @3 @masil-columns-count 1".into(),
            ],
        );
        assert_eq!(command[0], "if-shell");
        assert_eq!(command[4], format!("#{{==:{SIGNATURE},%1.0.0.80.40:}}"));
        assert!(command[5].ends_with("display-message -p columns_applied"));
        assert_eq!(command[6], "display-message -p columns_stale");
        assert!(single_quoted("a'b").is_err());
        assert_eq!(single_quoted("{\"V\":2}").unwrap(), "'{\"V\":2}'");
    }

    #[test]
    fn a_prev_in_the_old_format_keeps_its_widths() {
        let old =
            "a1b2,162x40,0,0{50x40,0,0,1,80x40,51,0,2,30x40,132,0[30x20,132,0,3,30x19,132,21,4]}";
        let prev = Layout::parse_prev(old).unwrap();
        assert_eq!(prev.widths(), [50, 80, 30]);
        assert_eq!(
            prev.column_panes(),
            [vec!["%1"], vec!["%2"], vec!["%3", "%4"]]
        );
        // A split of %2 in the current layout keeps %2's old width.
        let current = parse(&layout(
            &[
                (50, &[("%1", 40)]),
                (40, &[("%2", 40)]),
                (39, &[("%5", 40)]),
                (30, &[("%3", 20), ("%4", 19)]),
            ],
            40,
            &[],
        ));
        let built = build(&current, Some(&prev), &request(70)).unwrap();
        assert_eq!(built.widths, [50, 80, 70, 30]);
        assert!(Layout::parse_prev("0000,").is_err());
        assert!(Layout::parse_prev("a1b2,10x10,0,0").is_err());
        assert!(Layout::parse_prev("a1b2,10x10,0,0,1,").is_err());
    }

    #[test]
    fn the_window_read_checks_the_same_signature_as_an_apply() {
        assert!(WINDOW_FORMAT.contains(&format!("\t{SIGNATURE}\t")));
        assert_eq!(WINDOW_FORMAT.split('\t').count(), 16);
    }

    #[test]
    fn width_values_are_read_and_applied() {
        assert_eq!(WidthChange::parse("80"), Ok(WidthChange::Set(80)));
        assert_eq!(WidthChange::parse("+5"), Ok(WidthChange::Wider(5)));
        assert_eq!(WidthChange::parse("-5"), Ok(WidthChange::Narrower(5)));
        assert_eq!(WidthChange::parse("50%"), Ok(WidthChange::Percent(50)));
        for bad in ["0", "101%", "-t", "x", "", "+", "%", "20000"] {
            assert!(WidthChange::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(WidthChange::Narrower(500).apply(80, None), Some(0));
        assert_eq!(WidthChange::Percent(50).apply(80, Some(161)), Some(80));
        assert_eq!(WidthChange::Percent(50).apply(80, None), None);
    }

    #[test]
    fn options_take_only_what_a_command_allows() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let parsed = options(
            &args(&["-t", "%3", "--width", "60", "-d", "--", "vim", "-c", "x"]),
            &["-t", "--width", "-d", "--"],
        )
        .unwrap();
        assert_eq!(parsed.target.as_deref(), Some("%3"));
        assert_eq!(parsed.width, Some(60));
        assert!(parsed.detached);
        assert_eq!(parsed.command.unwrap(), ["vim", "-c", "x"]);
        for bad in [
            &["--force"][..],
            &["-t"],
            &["-t", "%1", "-t", "%2"],
            &["--width", "0"],
            &["--"],
        ] {
            assert!(
                options(&args(bad), &["-t", "--width", "--"]).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn pans_pick_the_clients_that_show_the_window() {
        let window = pan_window();
        assert!(window.starts_with(
            "run-shell -C \"#{L:#{?#{&&:#{==:#{client_session},#{session_name}},#{window_active}},"
        ));
        assert!(pan_all().starts_with("#{L:#{S:#{?#{==:#{session_name},#{client_session}},"));
        for text in [window, pan_all()] {
            assert_eq!(text.matches('{').count(), text.matches('}').count());
        }
        let hooks = hooks();
        let layout_hook = &hooks
            .iter()
            .find(|(name, _)| *name == "window-layout-changed")
            .unwrap()
            .1;
        assert!(layout_hook.contains("set -w @masil-columns-dirty 1"));
        assert!(
            layout_hook
                .contains("if -F \"#{@masil-columns-dirty}\" {} { set -wF @masil-columns-prev")
        );
        let attached = &hooks
            .iter()
            .find(|(name, _)| *name == "client-attached")
            .unwrap()
            .1;
        assert!(attached.starts_with("set -gu @masil-columns-pending ; "));
    }

    #[test]
    fn a_swap_that_keeps_the_active_pane_pans_and_a_drag_does_not() {
        let hooks = hooks();
        let hook = |wanted: &str| {
            hooks
                .iter()
                .find(|(name, _)| *name == wanted)
                .unwrap()
                .1
                .clone()
        };
        let place = place();
        assert_eq!(
            place,
            "#{P/i:#{pane_left}.#{pane_top}.#{pane_width}.#{pane_height}:}|#{pane_id}|#{pane_left}"
        );
        let keep_place = format!("set -wF @masil-columns-place \"{place}\"");
        // Focus: pan, keep where the active pane is, keep prev on the same count.
        let focus = hook("window-pane-changed");
        assert!(focus.contains(&format!("{} ; {keep_place} ; if -F ", pan_window())));
        assert!(focus.contains("set -wF @masil-columns-prev"));
        // Layout, same count: pan only when the cells and the active pane are
        // as kept and the pane's left edge is not (a drag changes the cells).
        let layout = hook("window-layout-changed");
        let swapped = format!(
            "if -F \"#{{&&:#{{m:{CELLS}|#{{pane_id}}|*,#{{@masil-columns-place}}}},#{{!=:#{{@masil-columns-place}},{place}}}}}\" {{ {} }} ; {keep_place} ; ",
            pan_window()
        );
        assert!(layout.contains(&swapped));
        assert_eq!(layout.matches("run-shell -C").count(), 1);
        let options = option_commands("@1", 2, None).unwrap();
        assert!(options.contains(&format!("set -wF -t @1 @masil-columns-place '{place}'")));
        assert!(WINDOW_OPTIONS.contains(&"@masil-columns-place"));
    }

    #[test]
    fn off_puts_back_the_saved_columns_with_current_places_and_marks() {
        // Saved before `on`: %1 | %2 over %3, list order %1 %2 %3.
        let saved = r#"{"V":2,"L":{"t":"h","w":100,"h":40,"x":0,"y":0,"c":[
            {"t":"p","w":50,"h":40,"x":0,"y":0,"i":0,"a":true,"I":"%1"},
            {"t":"v","w":49,"h":40,"x":51,"y":0,"c":[
                {"t":"p","w":49,"h":20,"x":51,"y":0,"i":1,"I":"%2"},
                {"t":"p","w":49,"h":19,"x":51,"y":21,"i":2,"l":0,"I":"%3"}]}]}}"#;
        // Now: swap-pane made the list %3 %2 %1, %2 is active and %1 last,
        // and a floating pane %9 came with `float`.
        let current = r#"{"V":2,"L":{"t":"h","w":200,"h":40,"x":0,"y":0,"c":[
            {"t":"p","w":80,"h":40,"x":0,"y":0,"i":2,"l":0,"I":"%1"},
            {"t":"v","w":80,"h":40,"x":81,"y":0,"c":[
                {"t":"p","w":80,"h":20,"x":81,"y":0,"i":1,"a":true,"I":"%2"},
                {"t":"p","w":80,"h":19,"x":81,"y":21,"i":0,"I":"%3"}]},
            {"t":"p","w":30,"h":8,"x":150,"y":5,"i":3,"z":0,"I":"%9"}]}}"#;
        let current = parse(current);
        let text = off_layout(saved, &current).unwrap().unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let mut found = Vec::new();
        fn collect(cell: &Value, found: &mut Vec<(String, u64, Option<u64>, bool)>) {
            match cell["c"].as_array() {
                Some(children) => children.iter().for_each(|child| collect(child, found)),
                None => found.push((
                    cell["I"].as_str().unwrap().into(),
                    cell["i"].as_u64().unwrap(),
                    cell["l"].as_u64(),
                    cell.get("a").is_some(),
                )),
            }
        }
        collect(&value["L"], &mut found);
        found.sort();
        assert_eq!(
            found,
            [
                ("%1".into(), 2, Some(0), false),
                ("%2".into(), 1, None, false),
                ("%3".into(), 0, None, false),
                ("%9".into(), 3, None, false),
            ]
        );
        // The saved widths are back and the floating pane is inside.
        let back = parse(&text);
        assert_eq!(back.widths(), [50, 49]);
        let floating = value["L"]["c"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cell| cell["I"] == "%9")
            .unwrap();
        assert!(floating["x"].as_u64().unwrap() + 30 < 100, "{floating}");
        // Another tiled pane set keeps the layout.
        let other = parse(&layout(
            &[(50, &[("%1", 40)]), (49, &[("%4", 40)])],
            40,
            &[],
        ));
        assert_eq!(off_layout(saved, &other).unwrap(), None);
    }
}
