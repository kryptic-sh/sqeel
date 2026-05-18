/// Schema tree node for the schema browser panel.
///
/// `tables_loaded_at` / `columns_loaded_at` record when the current session
/// last fetched that subtree from the server. `None` means not-yet-loaded;
/// a stale `Some` (older than the configured TTL) triggers a silent refresh
/// on next access. Both fields are `#[serde(skip)]` so cached trees always
/// load as "not yet fetched", forcing an initial refresh.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum SchemaNode {
    Database {
        name: String,
        expanded: bool,
        tables: Vec<SchemaNode>,
        #[serde(skip)]
        tables_loaded_at: Option<std::time::Instant>,
    },
    Table {
        name: String,
        expanded: bool,
        columns: Vec<SchemaNode>,
        #[serde(skip)]
        columns_loaded_at: Option<std::time::Instant>,
        /// Indexes associated with this table (each `SchemaNode::Index`).
        #[serde(default)]
        indexes: Vec<SchemaNode>,
        /// Foreign keys defined on this table (each `SchemaNode::ForeignKey`).
        #[serde(default)]
        foreign_keys: Vec<SchemaNode>,
        /// When indexes/FK data was last fetched; independent of columns_loaded_at.
        #[serde(skip)]
        relations_loaded_at: Option<std::time::Instant>,
        /// Whether the Indexes section is currently expanded in the tree.
        #[serde(default)]
        indexes_expanded: bool,
        /// Whether the References (FK) section is currently expanded in the tree.
        #[serde(default)]
        foreign_keys_expanded: bool,
    },
    Column {
        name: String,
        type_name: String,
        nullable: bool,
        is_pk: bool,
    },
    /// A single index on a table.
    Index {
        name: String,
        cols: Vec<String>,
        unique: bool,
    },
    /// A single foreign key constraint on a table.
    ForeignKey {
        name: String,
        cols: Vec<String>,
        ref_table: String,
        ref_cols: Vec<String>,
    },
}

/// Which subgroup of a Table node to toggle (Indexes or ForeignKeys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubGroup {
    Indexes,
    ForeignKeys,
}

/// Toggle the expanded flag for a Table subgroup (Indexes / ForeignKeys section).
/// Walks `node` to find the Table at the head of `path` and flips the
/// appropriate flag. No-op if the path doesn't resolve to a Table.
pub fn toggle_subgroup(nodes: &mut [SchemaNode], path: &[usize], group: SubGroup) {
    if path.is_empty() {
        return;
    }
    let idx = path[0];
    if idx >= nodes.len() {
        return;
    }
    if path.len() == 1 {
        if let SchemaNode::Table {
            indexes_expanded,
            foreign_keys_expanded,
            ..
        } = &mut nodes[idx]
        {
            match group {
                SubGroup::Indexes => *indexes_expanded = !*indexes_expanded,
                SubGroup::ForeignKeys => *foreign_keys_expanded = !*foreign_keys_expanded,
            }
        }
        return;
    }
    match &mut nodes[idx] {
        SchemaNode::Database { tables, .. } => toggle_subgroup(tables, &path[1..], group),
        SchemaNode::Table { columns, .. } => toggle_subgroup(columns, &path[1..], group),
        _ => {}
    }
}

/// Returns true if `at` is set and no older than `ttl`. `ttl == 0` means
/// "never expire".
pub fn is_fresh(at: Option<std::time::Instant>, ttl: std::time::Duration) -> bool {
    match at {
        Some(_) if ttl.is_zero() => true,
        Some(t) => t.elapsed() < ttl,
        None => false,
    }
}

impl SchemaNode {
    pub fn name(&self) -> &str {
        match self {
            SchemaNode::Database { name, .. } => name,
            SchemaNode::Table { name, .. } => name,
            SchemaNode::Column { name, .. } => name,
            SchemaNode::Index { name, .. } => name,
            SchemaNode::ForeignKey { name, .. } => name,
        }
    }

    pub fn is_expanded(&self) -> bool {
        match self {
            SchemaNode::Database { expanded, .. } => *expanded,
            SchemaNode::Table { expanded, .. } => *expanded,
            SchemaNode::Column { .. } => false,
            SchemaNode::Index { .. } => false,
            SchemaNode::ForeignKey { .. } => false,
        }
    }

    pub fn toggle(&mut self) {
        match self {
            SchemaNode::Database { expanded, .. } => *expanded = !*expanded,
            SchemaNode::Table { expanded, .. } => *expanded = !*expanded,
            SchemaNode::Column { .. } => {}
            SchemaNode::Index { .. } => {}
            SchemaNode::ForeignKey { .. } => {}
        }
    }
}

/// Classifies a tree item so renderers can style icon/type without parsing
/// the label back apart.
#[derive(Debug, Clone)]
pub enum SchemaItemKind {
    Database,
    Table,
    Column {
        type_name: String,
        is_pk: bool,
    },
    /// Non-interactive hint row emitted under an expanded node with no
    /// children. `loading == true` means the lazy loader hasn't replied
    /// yet (render path can show an animated spinner); `false` means
    /// the load came back empty ("(no tables)" / "(no columns)").
    Placeholder {
        loading: bool,
    },
    /// Collapsible header for the Indexes sub-section under a Table.
    IndexGroup {
        count: usize,
    },
    /// A single index entry under an IndexGroup header.
    Index {
        unique: bool,
        cols: Vec<String>,
    },
    /// Collapsible header for the References (FK) sub-section under a Table.
    ForeignKeyGroup {
        count: usize,
    },
    /// A single foreign-key entry under a ForeignKeyGroup header.
    ForeignKey {
        ref_table: String,
        ref_cols: Vec<String>,
    },
}

/// Flat list of visible tree items for rendering.
#[derive(Debug, Clone)]
pub struct SchemaTreeItem {
    pub label: String,
    pub depth: usize,
    pub node_path: Vec<usize>, // indices to reach this node from root
    pub name: String,
    pub kind: SchemaItemKind,
}

pub fn node_icon_char(node: &SchemaNode) -> &'static str {
    match node {
        SchemaNode::Database { .. } => "󰆼",
        SchemaNode::Table { .. } => "󰓫",
        SchemaNode::Column { is_pk: true, .. } => "󰌆",
        SchemaNode::Column { .. } => "󱘚",
        SchemaNode::Index { .. } => "󰠞",
        SchemaNode::ForeignKey { .. } => "󰈩",
    }
}

pub fn item_kind(node: &SchemaNode) -> SchemaItemKind {
    match node {
        SchemaNode::Database { .. } => SchemaItemKind::Database,
        SchemaNode::Table { .. } => SchemaItemKind::Table,
        SchemaNode::Column {
            type_name, is_pk, ..
        } => SchemaItemKind::Column {
            type_name: type_name.clone(),
            is_pk: *is_pk,
        },
        SchemaNode::Index { unique, cols, .. } => SchemaItemKind::Index {
            unique: *unique,
            cols: cols.clone(),
        },
        SchemaNode::ForeignKey {
            ref_table,
            ref_cols,
            ..
        } => SchemaItemKind::ForeignKey {
            ref_table: ref_table.clone(),
            ref_cols: ref_cols.clone(),
        },
    }
}

pub fn flatten_tree(nodes: &[SchemaNode]) -> Vec<SchemaTreeItem> {
    let mut items = Vec::new();
    flatten_nodes(nodes, 0, &[], &[], &mut items);
    items
}

/// Flatten ALL nodes regardless of expanded state, using simple depth indentation.
/// Used for search so collapsed subtrees are still searchable.
pub fn flatten_all(nodes: &[SchemaNode]) -> Vec<SchemaTreeItem> {
    let mut items = Vec::new();
    flatten_nodes_all(nodes, 0, &[], &mut items);
    items
}

fn flatten_nodes_all(
    nodes: &[SchemaNode],
    depth: usize,
    path: &[usize],
    items: &mut Vec<SchemaTreeItem>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let mut node_path = path.to_vec();
        node_path.push(i);
        let indent = " ".repeat(1 + depth * 2);
        let icon = node_icon_char(node);
        let name = node.name();
        let extra = match node {
            SchemaNode::Column { type_name, .. } if !type_name.is_empty() => {
                format!(": {type_name}")
            }
            _ => String::new(),
        };
        let label = format!("{indent}{icon} {name}{extra}");
        items.push(SchemaTreeItem {
            label,
            depth,
            node_path: node_path.clone(),
            name: name.to_string(),
            kind: item_kind(node),
        });
        match node {
            SchemaNode::Database { tables, .. } => {
                flatten_nodes_all(tables, depth + 1, &node_path, items);
            }
            SchemaNode::Table {
                columns,
                indexes,
                foreign_keys,
                ..
            } => {
                flatten_nodes_all(columns, depth + 1, &node_path, items);
                // Emit index and FK nodes (always expanded for search purposes).
                if !indexes.is_empty() {
                    let group_indent = " ".repeat(1 + (depth + 1) * 2);
                    let group_label = format!("{group_indent}▸ Indexes ({})", indexes.len());
                    items.push(SchemaTreeItem {
                        label: group_label.clone(),
                        depth: depth + 1,
                        node_path: node_path.clone(),
                        name: group_label,
                        kind: SchemaItemKind::IndexGroup {
                            count: indexes.len(),
                        },
                    });
                    flatten_nodes_all(indexes, depth + 2, &node_path, items);
                }
                if !foreign_keys.is_empty() {
                    let group_indent = " ".repeat(1 + (depth + 1) * 2);
                    let group_label =
                        format!("{group_indent}▸ References ({}) →", foreign_keys.len());
                    items.push(SchemaTreeItem {
                        label: group_label.clone(),
                        depth: depth + 1,
                        node_path: node_path.clone(),
                        name: group_label,
                        kind: SchemaItemKind::ForeignKeyGroup {
                            count: foreign_keys.len(),
                        },
                    });
                    flatten_nodes_all(foreign_keys, depth + 2, &node_path, items);
                }
            }
            _ => {}
        }
    }
}

fn flatten_nodes(
    nodes: &[SchemaNode],
    depth: usize,
    path: &[usize],
    _ancestor_is_last: &[bool],
    items: &mut Vec<SchemaTreeItem>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let mut node_path = path.to_vec();
        node_path.push(i);

        let indent = " ".repeat(1 + depth * 2);
        let icon = node_icon_char(node);
        let name = node.name();
        let extra = match node {
            SchemaNode::Column { type_name, .. } if !type_name.is_empty() => {
                format!(": {type_name}")
            }
            SchemaNode::Index { unique, cols, .. } => {
                let unique_badge = if *unique { " UNIQUE" } else { "" };
                let col_list = cols.join(", ");
                format!("{unique_badge} ({col_list})")
            }
            SchemaNode::ForeignKey {
                cols,
                ref_table,
                ref_cols,
                ..
            } => {
                let col_list = cols.join(", ");
                let ref_list = ref_cols.join(", ");
                format!(" ({col_list}) → {ref_table}({ref_list})")
            }
            _ => String::new(),
        };
        let label = format!("{indent}{icon} {name}{extra}");

        items.push(SchemaTreeItem {
            label,
            depth,
            node_path: node_path.clone(),
            name: name.to_string(),
            kind: item_kind(node),
        });

        let child_ancestor_is_last: Vec<bool> = Vec::new();

        match node {
            SchemaNode::Database {
                expanded: true,
                tables,
                tables_loaded_at,
                ..
            } => {
                if tables.is_empty() {
                    items.push(placeholder_item(
                        depth + 1,
                        &node_path,
                        tables_loaded_at.is_some(),
                        "tables",
                    ));
                } else {
                    flatten_nodes(
                        tables,
                        depth + 1,
                        &node_path,
                        &child_ancestor_is_last,
                        items,
                    );
                }
            }
            SchemaNode::Table {
                expanded: true,
                columns,
                columns_loaded_at,
                indexes,
                foreign_keys,
                indexes_expanded,
                foreign_keys_expanded,
                ..
            } => {
                if columns.is_empty() {
                    items.push(placeholder_item(
                        depth + 1,
                        &node_path,
                        columns_loaded_at.is_some(),
                        "columns",
                    ));
                } else {
                    flatten_nodes(
                        columns,
                        depth + 1,
                        &node_path,
                        &child_ancestor_is_last,
                        items,
                    );
                }
                // Emit Indexes section header (only when indexes are present).
                if !indexes.is_empty() {
                    let group_indent = " ".repeat(1 + (depth + 1) * 2);
                    let arrow = if *indexes_expanded { "▾" } else { "▸" };
                    let group_label = format!("{group_indent}{arrow} Indexes ({})", indexes.len());
                    items.push(SchemaTreeItem {
                        label: group_label.clone(),
                        depth: depth + 1,
                        node_path: node_path.clone(),
                        name: group_label,
                        kind: SchemaItemKind::IndexGroup {
                            count: indexes.len(),
                        },
                    });
                    if *indexes_expanded {
                        flatten_nodes(
                            indexes,
                            depth + 2,
                            &node_path,
                            &child_ancestor_is_last,
                            items,
                        );
                    }
                }
                // Emit ForeignKeys section header (only when FKs are present).
                if !foreign_keys.is_empty() {
                    let group_indent = " ".repeat(1 + (depth + 1) * 2);
                    let arrow = if *foreign_keys_expanded { "▾" } else { "▸" };
                    let group_label =
                        format!("{group_indent}{arrow} References ({})", foreign_keys.len());
                    items.push(SchemaTreeItem {
                        label: group_label.clone(),
                        depth: depth + 1,
                        node_path: node_path.clone(),
                        name: group_label,
                        kind: SchemaItemKind::ForeignKeyGroup {
                            count: foreign_keys.len(),
                        },
                    });
                    if *foreign_keys_expanded {
                        flatten_nodes(
                            foreign_keys,
                            depth + 2,
                            &node_path,
                            &child_ancestor_is_last,
                            items,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn placeholder_item(
    depth: usize,
    parent_path: &[usize],
    loaded: bool,
    what: &str,
) -> SchemaTreeItem {
    let indent = " ".repeat(1 + depth * 2);
    let text = if loaded {
        format!("(no {what})")
    } else {
        format!("loading {what}…")
    };
    // Sentinel path that won't match any real node — toggle_node /
    // path_to_string safely no-op on out-of-range indices.
    let mut node_path = parent_path.to_vec();
    node_path.push(usize::MAX);
    SchemaTreeItem {
        label: format!("{indent}{text}"),
        depth,
        node_path,
        name: text.clone(),
        kind: SchemaItemKind::Placeholder { loading: !loaded },
    }
}

/// Subsequence (fuzzy) match: every char of `query` appears in `label` in order.
/// `query` must already be lowercase; `label` gets trimmed + lowered here.
pub fn label_matches(label: &str, query_lower: &str) -> bool {
    let name = label.trim().to_lowercase();
    let mut chars = name.chars();
    query_lower.chars().all(|qc| chars.any(|lc| lc == qc))
}

/// Filter `all` to items whose label fuzzy-matches `query`, plus all ancestors
/// (so the tree stays navigable) and all descendants of matches (so matching a
/// table keeps its columns visible). Returns items in original order.
pub fn filter_items<'a>(all: &'a [SchemaTreeItem], query: &str) -> Vec<&'a SchemaTreeItem> {
    let q = query.to_lowercase();
    let mut ancestors: std::collections::HashSet<Vec<usize>> = std::collections::HashSet::new();
    let mut matched: Vec<Vec<usize>> = Vec::new();
    for item in all.iter().filter(|it| label_matches(&it.label, &q)) {
        for len in 1..=item.node_path.len() {
            ancestors.insert(item.node_path[..len].to_vec());
        }
        matched.push(item.node_path.clone());
    }
    let is_descendant = |path: &[usize]| {
        matched
            .iter()
            .any(|m| path.len() > m.len() && path[..m.len()] == m[..])
    };
    all.iter()
        .filter(|it| ancestors.contains(&it.node_path) || is_descendant(&it.node_path))
        .collect()
}

/// Copy `expanded` flags from `old` into `new` by matching node names at each level.
/// Called before replacing schema nodes on a background refresh so the user's
/// open/closed state is preserved.
pub fn merge_expansion(old: &[SchemaNode], new: &mut [SchemaNode]) {
    for new_node in new.iter_mut() {
        let Some(old_node) = old.iter().find(|o| o.name() == new_node.name()) else {
            continue;
        };
        match (old_node, new_node) {
            (
                SchemaNode::Database {
                    expanded: old_exp,
                    tables: old_tables,
                    ..
                },
                SchemaNode::Database {
                    expanded: new_exp,
                    tables: new_tables,
                    ..
                },
            ) => {
                *new_exp = *old_exp;
                merge_expansion(old_tables, new_tables);
            }
            (
                SchemaNode::Table {
                    expanded: old_exp,
                    columns: old_cols,
                    indexes_expanded: old_idx_exp,
                    foreign_keys_expanded: old_fk_exp,
                    ..
                },
                SchemaNode::Table {
                    expanded: new_exp,
                    columns: new_cols,
                    indexes_expanded: new_idx_exp,
                    foreign_keys_expanded: new_fk_exp,
                    ..
                },
            ) => {
                *new_exp = *old_exp;
                *new_idx_exp = *old_idx_exp;
                *new_fk_exp = *old_fk_exp;
                merge_expansion(old_cols, new_cols);
            }
            _ => {}
        }
    }
}

/// Walk `node_path` indices through the tree and return the joined name string, e.g. `"mydb/users/id"`.
pub fn path_to_string(path: &[usize], nodes: &[SchemaNode]) -> String {
    let mut parts = Vec::new();
    let mut current = nodes;
    for &idx in path {
        let Some(node) = current.get(idx) else {
            break;
        };
        parts.push(node.name().to_string());
        match node {
            SchemaNode::Database { tables, .. } => current = tables,
            SchemaNode::Table { columns, .. } => current = columns,
            SchemaNode::Column { .. } => break,
            SchemaNode::Index { .. } => break,
            SchemaNode::ForeignKey { .. } => break,
        }
    }
    parts.join("/")
}

/// Find the flat-list index of the visible item whose tree path matches `path_str`.
pub fn find_cursor_by_path(
    items: &[SchemaTreeItem],
    nodes: &[SchemaNode],
    path_str: &str,
) -> Option<usize> {
    items
        .iter()
        .position(|item| path_to_string(&item.node_path, nodes) == path_str)
}

/// Expand all ancestor nodes needed so the item at `path_str` becomes visible.
/// E.g. for `"mydb/users/id"` this expands the `mydb` database and the `users` table.
pub fn expand_path(nodes: &mut [SchemaNode], path_str: &str) {
    let parts: Vec<&str> = path_str.splitn(3, '/').collect();
    // Need to expand: Database for parts[0] (when parts.len() >= 2),
    // and Table for parts[1] inside that db (when parts.len() >= 3).
    if parts.len() < 2 {
        return;
    }
    for node in nodes.iter_mut() {
        if let SchemaNode::Database {
            name,
            expanded,
            tables,
            ..
        } = node
            && name == parts[0]
        {
            *expanded = true;
            if parts.len() >= 3 {
                for table in tables.iter_mut() {
                    if let SchemaNode::Table {
                        name: tname,
                        expanded: texpanded,
                        ..
                    } = table
                        && tname == parts[1]
                    {
                        *texpanded = true;
                        break;
                    }
                }
            }
            break;
        }
    }
}

/// Collect path strings for every expanded Database/Table node, e.g. `["mydb", "mydb/users"]`.
pub fn collect_expanded_paths(nodes: &[SchemaNode]) -> Vec<String> {
    let mut paths = Vec::new();
    for node in nodes {
        if let SchemaNode::Database {
            name,
            expanded: true,
            tables,
            ..
        } = node
        {
            paths.push(name.clone());
            for table in tables {
                if let SchemaNode::Table {
                    name: tname,
                    expanded: true,
                    ..
                } = table
                {
                    paths.push(format!("{name}/{tname}"));
                }
            }
        }
    }
    paths
}

/// Expand the nodes named by each path string (inverse of `collect_expanded_paths`).
pub fn restore_expanded_paths(nodes: &mut [SchemaNode], paths: &[String]) {
    for path in paths {
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        match parts.as_slice() {
            [db_name] => {
                for node in nodes.iter_mut() {
                    if let SchemaNode::Database { name, expanded, .. } = node
                        && name == db_name
                    {
                        *expanded = true;
                        break;
                    }
                }
            }
            [db_name, table_name] => {
                for node in nodes.iter_mut() {
                    if let SchemaNode::Database { name, tables, .. } = node
                        && name == db_name
                    {
                        for table in tables.iter_mut() {
                            if let SchemaNode::Table {
                                name: tname,
                                expanded,
                                ..
                            } = table
                                && tname == table_name
                            {
                                *expanded = true;
                                break;
                            }
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn toggle_node(nodes: &mut [SchemaNode], path: &[usize]) {
    if path.is_empty() {
        return;
    }
    let idx = path[0];
    if idx >= nodes.len() {
        return;
    }
    if path.len() == 1 {
        nodes[idx].toggle();
        return;
    }
    match &mut nodes[idx] {
        SchemaNode::Database { tables, .. } => toggle_node(tables, &path[1..]),
        SchemaNode::Table { columns, .. } => toggle_node(columns, &path[1..]),
        _ => {}
    }
}

/// Given a flat visible item list and a `ForeignKey` item, find the flat-list
/// index of the referenced table (`ref_table`) within the same database
/// subtree. Returns `None` if the table isn't visible in the current tree.
pub fn fk_jump_target(
    items: &[SchemaTreeItem],
    _fk_item: &SchemaTreeItem,
    ref_table: &str,
) -> Option<usize> {
    // The FK item's path is [db_idx, table_idx, fk_idx] (or similar).
    // Walk items to find a Table whose name matches ref_table.
    items
        .iter()
        .position(|it| matches!(&it.kind, SchemaItemKind::Table) && it.name == ref_table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> Vec<SchemaNode> {
        vec![SchemaNode::Database {
            name: "mydb".into(),
            expanded: false,
            tables: vec![SchemaNode::Table {
                name: "users".into(),
                expanded: false,
                columns: vec![SchemaNode::Column {
                    name: "id".into(),
                    type_name: "INT".into(),
                    nullable: false,
                    is_pk: true,
                }],
                columns_loaded_at: Some(std::time::Instant::now()),
                indexes: vec![],
                foreign_keys: vec![],
                relations_loaded_at: None,
                indexes_expanded: false,
                foreign_keys_expanded: false,
            }],
            tables_loaded_at: Some(std::time::Instant::now()),
        }]
    }

    fn sample_tree_with_relations() -> Vec<SchemaNode> {
        vec![SchemaNode::Database {
            name: "mydb".into(),
            expanded: true,
            tables: vec![
                SchemaNode::Table {
                    name: "roles".into(),
                    expanded: false,
                    columns: vec![SchemaNode::Column {
                        name: "id".into(),
                        type_name: "INT".into(),
                        nullable: false,
                        is_pk: true,
                    }],
                    columns_loaded_at: Some(std::time::Instant::now()),
                    indexes: vec![],
                    foreign_keys: vec![],
                    relations_loaded_at: None,
                    indexes_expanded: false,
                    foreign_keys_expanded: false,
                },
                SchemaNode::Table {
                    name: "users".into(),
                    expanded: true,
                    columns: vec![
                        SchemaNode::Column {
                            name: "id".into(),
                            type_name: "INT".into(),
                            nullable: false,
                            is_pk: true,
                        },
                        SchemaNode::Column {
                            name: "email".into(),
                            type_name: "VARCHAR".into(),
                            nullable: false,
                            is_pk: false,
                        },
                        SchemaNode::Column {
                            name: "role_id".into(),
                            type_name: "INT".into(),
                            nullable: false,
                            is_pk: false,
                        },
                    ],
                    columns_loaded_at: Some(std::time::Instant::now()),
                    indexes: vec![
                        SchemaNode::Index {
                            name: "idx_users_email".into(),
                            cols: vec!["email".into()],
                            unique: true,
                        },
                        SchemaNode::Index {
                            name: "idx_users_role".into(),
                            cols: vec!["role_id".into()],
                            unique: false,
                        },
                    ],
                    foreign_keys: vec![SchemaNode::ForeignKey {
                        name: "fk_users_role_id".into(),
                        cols: vec!["role_id".into()],
                        ref_table: "roles".into(),
                        ref_cols: vec!["id".into()],
                    }],
                    relations_loaded_at: Some(std::time::Instant::now()),
                    indexes_expanded: false,
                    foreign_keys_expanded: false,
                },
            ],
            tables_loaded_at: Some(std::time::Instant::now()),
        }]
    }

    #[test]
    fn flatten_collapsed_shows_only_databases() {
        let tree = sample_tree();
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
        assert!(items[0].label.contains("mydb"));
    }

    #[test]
    fn expand_database_shows_tables() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]);
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 2);
        assert!(items[1].label.contains("users"));
    }

    #[test]
    fn expand_table_shows_columns() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]); // expand db
        toggle_node(&mut tree, &[0, 0]); // expand table
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 3);
        assert!(items[2].label.contains("id"));
    }

    #[test]
    fn expanded_empty_database_shows_no_tables_placeholder() {
        let tree = vec![SchemaNode::Database {
            name: "empty".into(),
            expanded: true,
            tables: vec![],
            tables_loaded_at: Some(std::time::Instant::now()),
        }];
        // (no changes needed — Database variant unchanged)
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1].kind, SchemaItemKind::Placeholder { .. }));
        assert!(items[1].label.contains("no tables"));
    }

    #[test]
    fn expanded_loading_database_shows_loading_placeholder() {
        let tree = vec![SchemaNode::Database {
            name: "loading".into(),
            expanded: true,
            tables: vec![],
            tables_loaded_at: None,
        }];
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 2);
        assert!(items[1].label.contains("loading"));
    }

    #[test]
    fn expanded_empty_table_shows_no_columns_placeholder() {
        let tree = vec![SchemaNode::Database {
            name: "db".into(),
            expanded: true,
            tables: vec![SchemaNode::Table {
                name: "empty".into(),
                expanded: true,
                columns: vec![],
                columns_loaded_at: Some(std::time::Instant::now()),
                indexes: vec![],
                foreign_keys: vec![],
                relations_loaded_at: None,
                indexes_expanded: false,
                foreign_keys_expanded: false,
            }],
            tables_loaded_at: Some(std::time::Instant::now()),
        }];
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 3);
        assert!(items[2].label.contains("no columns"));
        assert!(matches!(items[2].kind, SchemaItemKind::Placeholder { .. }));
    }

    #[test]
    fn collapse_database_hides_children() {
        let mut tree = sample_tree();
        toggle_node(&mut tree, &[0]); // expand
        toggle_node(&mut tree, &[0]); // collapse
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn filter_items_includes_ancestors() {
        let all = flatten_all(&sample_tree());
        let filtered = filter_items(&all, "id");
        // Match on "id" column must pull in mydb + users ancestors.
        assert_eq!(filtered.len(), 3);
        assert!(filtered[0].label.contains("mydb"));
        assert!(filtered[1].label.contains("users"));
        assert!(filtered[2].label.contains("id"));
    }

    #[test]
    fn filter_items_includes_descendants_of_match() {
        let all = flatten_all(&sample_tree());
        let filtered = filter_items(&all, "users");
        // Match on "users" table must keep its "id" column visible too.
        assert_eq!(filtered.len(), 3);
        assert!(filtered[0].label.contains("mydb"));
        assert!(filtered[1].label.contains("users"));
        assert!(filtered[2].label.contains("id"));
    }

    #[test]
    fn filter_items_empty_on_no_match() {
        let all = flatten_all(&sample_tree());
        assert!(filter_items(&all, "zzz").is_empty());
    }

    #[test]
    fn cursor_bounds_respected() {
        let tree = sample_tree();
        let items = flatten_tree(&tree);
        assert_eq!(items.len(), 1);
        // Cursor cannot go below 0 or above items.len()-1
        let cursor: usize = 0;
        let next = cursor.saturating_add(1).min(items.len().saturating_sub(1));
        assert_eq!(next, 0); // only 1 item, stays at 0
    }

    // ── New tests for indexes + foreign key groups ────────────────────────

    #[test]
    fn flatten_table_with_indexes_and_fks_emits_group_headers() {
        // mydb is expanded, users table is expanded
        let tree = sample_tree_with_relations();
        let items = flatten_tree(&tree);
        // Expected:
        //  [0] mydb (db)
        //  [1] roles (table — collapsed)
        //  [2] users (table — expanded)
        //  [3] id (col)
        //  [4] email (col)
        //  [5] role_id (col)
        //  [6] ▸ Indexes (2)   ← IndexGroup header
        //  [7] ▸ References (1) ← ForeignKeyGroup header
        let kinds: Vec<&str> = items
            .iter()
            .map(|it| match &it.kind {
                SchemaItemKind::Database => "db",
                SchemaItemKind::Table => "table",
                SchemaItemKind::Column { .. } => "col",
                SchemaItemKind::IndexGroup { .. } => "idx_group",
                SchemaItemKind::ForeignKeyGroup { .. } => "fk_group",
                SchemaItemKind::Index { .. } => "idx",
                SchemaItemKind::ForeignKey { .. } => "fk",
                SchemaItemKind::Placeholder { .. } => "placeholder",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "db",
                "table",
                "table",
                "col",
                "col",
                "col",
                "idx_group",
                "fk_group"
            ]
        );
        // Count in the IndexGroup must be 2.
        assert!(matches!(
            items[6].kind,
            SchemaItemKind::IndexGroup { count: 2 }
        ));
        // Count in the ForeignKeyGroup must be 1.
        assert!(matches!(
            items[7].kind,
            SchemaItemKind::ForeignKeyGroup { count: 1 }
        ));
    }

    #[test]
    fn flatten_table_indexes_expanded_emits_index_rows() {
        let mut tree = sample_tree_with_relations();
        // Expand the indexes section on the users table (db[0].tables[1]).
        toggle_subgroup(&mut tree, &[0, 1], SubGroup::Indexes);
        let items = flatten_tree(&tree);
        // After expanding Indexes we should see two Index rows between the
        // IndexGroup header and the ForeignKeyGroup header.
        let kinds: Vec<&str> = items
            .iter()
            .map(|it| match &it.kind {
                SchemaItemKind::Database => "db",
                SchemaItemKind::Table => "table",
                SchemaItemKind::Column { .. } => "col",
                SchemaItemKind::IndexGroup { .. } => "idx_group",
                SchemaItemKind::ForeignKeyGroup { .. } => "fk_group",
                SchemaItemKind::Index { .. } => "idx",
                SchemaItemKind::ForeignKey { .. } => "fk",
                SchemaItemKind::Placeholder { .. } => "placeholder",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "db",
                "table",
                "table",
                "col",
                "col",
                "col",
                "idx_group",
                "idx",
                "idx",
                "fk_group"
            ]
        );
    }

    #[test]
    fn flatten_table_fk_expanded_emits_fk_rows() {
        let mut tree = sample_tree_with_relations();
        // Expand foreign_keys on users table.
        toggle_subgroup(&mut tree, &[0, 1], SubGroup::ForeignKeys);
        let items = flatten_tree(&tree);
        let kinds: Vec<&str> = items
            .iter()
            .map(|it| match &it.kind {
                SchemaItemKind::Database => "db",
                SchemaItemKind::Table => "table",
                SchemaItemKind::Column { .. } => "col",
                SchemaItemKind::IndexGroup { .. } => "idx_group",
                SchemaItemKind::ForeignKeyGroup { .. } => "fk_group",
                SchemaItemKind::Index { .. } => "idx",
                SchemaItemKind::ForeignKey { .. } => "fk",
                SchemaItemKind::Placeholder { .. } => "placeholder",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "db",
                "table",
                "table",
                "col",
                "col",
                "col",
                "idx_group",
                "fk_group",
                "fk"
            ]
        );
    }

    #[test]
    fn toggle_subgroup_flips_indexes_flag() {
        let mut tree = sample_tree_with_relations();
        // Initially false.
        if let SchemaNode::Table {
            indexes_expanded, ..
        } = &tree[0]
            .clone()
            .try_into_db_tables()
            .unwrap()
            .get(1)
            .unwrap()
        {
            assert!(!indexes_expanded);
        }
        toggle_subgroup(&mut tree, &[0, 1], SubGroup::Indexes);
        if let SchemaNode::Database { tables, .. } = &tree[0]
            && let SchemaNode::Table {
                indexes_expanded, ..
            } = &tables[1]
        {
            assert!(indexes_expanded);
        }
        toggle_subgroup(&mut tree, &[0, 1], SubGroup::Indexes);
        if let SchemaNode::Database { tables, .. } = &tree[0]
            && let SchemaNode::Table {
                indexes_expanded, ..
            } = &tables[1]
        {
            assert!(!indexes_expanded);
        }
    }

    #[test]
    fn toggle_subgroup_flips_foreign_keys_flag() {
        let mut tree = sample_tree_with_relations();
        toggle_subgroup(&mut tree, &[0, 1], SubGroup::ForeignKeys);
        if let SchemaNode::Database { tables, .. } = &tree[0]
            && let SchemaNode::Table {
                foreign_keys_expanded,
                ..
            } = &tables[1]
        {
            assert!(foreign_keys_expanded);
        }
    }

    impl SchemaNode {
        fn try_into_db_tables(self) -> Option<Vec<SchemaNode>> {
            if let SchemaNode::Database { tables, .. } = self {
                Some(tables)
            } else {
                None
            }
        }
    }

    #[test]
    fn fk_jump_target_finds_referenced_table() {
        let tree = sample_tree_with_relations();
        let items = flatten_tree(&tree);
        // The FK references "roles". Find its flat-list index.
        let target_idx = fk_jump_target(&items, &items[0], "roles");
        assert!(target_idx.is_some());
        let idx = target_idx.unwrap();
        assert_eq!(items[idx].name, "roles");
        assert!(matches!(items[idx].kind, SchemaItemKind::Table));
    }

    #[test]
    fn fk_jump_target_returns_none_for_missing_table() {
        let tree = sample_tree_with_relations();
        let items = flatten_tree(&tree);
        assert!(fk_jump_target(&items, &items[0], "nonexistent").is_none());
    }
}
