use crate::components::chat::{CodeBlock, Comment, LinkWithControls};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
// ---------------------------------------------------------------------------

/// Helper macro for routing inline markdown nodes.
/// Always appends to the innermost open formatting tag (like strong or emphasis).
/// If no tags are open, it appends to the root `current_inlines` vector.
macro_rules! active_target {
    ($stack:ident, $current:ident) => {
        if let Some((_, inlines)) = $stack.last_mut() {
            inlines
        } else {
            &mut $current
        }
    };
}

// ---------------------------------------------------------------------------
// Module-level IR type definitions
// Defined here (outside MarkdownRenderer) so DetailsBlock can reference them.
// ---------------------------------------------------------------------------

/// High-level structural blocks in the Markdown IR.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum MdBlock {
    Header {
        level: HeadingLevel,
        content: Vec<MdInline>,
    },
    Paragraph(Vec<MdInline>),
    List {
        items: Vec<MdListItem>,
        start: Option<u64>,
    },
    CodeBlock {
        lang: String,
        code: String,
    },
    Table {
        headers: Vec<Vec<MdInline>>,
        rows: Vec<Vec<Vec<MdInline>>>,
        alignments: Vec<Alignment>,
    },
    BlockQuote(Vec<MdBlock>),
    HorizontalRule,
    /// A `<details>/<summary>` expandable block.
    /// Parsed from raw HTML events emitted by pulldown_cmark.
    Details {
        id: String,
        summary: String,
        body: Vec<MdBlock>,
        default_open: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum MdInline {
    Text(String),
    Code(String),
    Link { href: String, text: String },
    Image { src: String, alt: String },
    SoftBreak,
    HardBreak,
    Emphasis(Vec<MdInline>),
    Strong(Vec<MdInline>),
    Strikethrough(Vec<MdInline>),
    TaskListMarker(bool),
}

#[derive(Debug, Clone, PartialEq)]
struct MdListItem {
    blocks: Vec<MdBlock>,
}

#[derive(Debug, Clone, PartialEq)]
enum InlineTag {
    Emphasis,
    Strong,
    Strikethrough,
}

#[derive(Debug, Clone, PartialEq)]
enum RenderNode {
    Text(String),
    Code(String),
    Link {
        href: String,
        text: String,
    },
    Image {
        src: String,
        alt: String,
    },
    SoftBreak,
    HardBreak,
    Emphasis(Vec<RenderNode>),
    Strong(Vec<RenderNode>),
    Strikethrough(Vec<RenderNode>),
    TaskListMarker(bool),
    CommentWrapped {
        children: Vec<RenderNode>,
        comment: Comment,
    },
    HighlightWrapped {
        children: Vec<RenderNode>,
    },
}

fn get_node_text(node: &RenderNode) -> String {
    match node {
        RenderNode::Text(s) | RenderNode::Code(s) => s.clone(),
        RenderNode::Link { text, .. } => text.clone(),
        RenderNode::Image { alt, .. } => alt.clone(),
        RenderNode::SoftBreak => " ".to_string(),
        RenderNode::HardBreak => "\n".to_string(),
        RenderNode::TaskListMarker(_) => String::new(),
        RenderNode::Emphasis(children)
        | RenderNode::Strong(children)
        | RenderNode::Strikethrough(children)
        | RenderNode::CommentWrapped { children, .. }
        | RenderNode::HighlightWrapped { children } => {
            children.iter().map(get_node_text).collect()
        }
    }
}

fn split_node(node: RenderNode, at: usize) -> (Option<RenderNode>, Option<RenderNode>) {
    let len = get_node_text(&node).len();
    if at == 0 {
        return (None, Some(node));
    }
    if at >= len {
        return (Some(node), None);
    }

    match node {
        RenderNode::Text(s) => {
            let (left, right) = s.split_at(at);
            (
                Some(RenderNode::Text(left.to_string())),
                Some(RenderNode::Text(right.to_string())),
            )
        }
        RenderNode::Code(s) => {
            let (left, right) = s.split_at(at);
            (
                Some(RenderNode::Code(left.to_string())),
                Some(RenderNode::Code(right.to_string())),
            )
        }
        RenderNode::Link { href, text } => {
            let (left_text, right_text) = text.split_at(at);
            (
                Some(RenderNode::Link {
                    href: href.clone(),
                    text: left_text.to_string(),
                }),
                Some(RenderNode::Link {
                    href,
                    text: right_text.to_string(),
                }),
            )
        }
        RenderNode::Image { src, alt } => (Some(RenderNode::Image { src, alt }), None),
        RenderNode::SoftBreak | RenderNode::HardBreak | RenderNode::TaskListMarker(_) => {
            (Some(node.clone()), None)
        }
        RenderNode::Emphasis(children) => {
            let (left, right) = split_children(children, at);
            (
                if left.is_empty() {
                    None
                } else {
                    Some(RenderNode::Emphasis(left))
                },
                if right.is_empty() {
                    None
                } else {
                    Some(RenderNode::Emphasis(right))
                },
            )
        }
        RenderNode::Strong(children) => {
            let (left, right) = split_children(children, at);
            (
                if left.is_empty() {
                    None
                } else {
                    Some(RenderNode::Strong(left))
                },
                if right.is_empty() {
                    None
                } else {
                    Some(RenderNode::Strong(right))
                },
            )
        }
        RenderNode::Strikethrough(children) => {
            let (left, right) = split_children(children, at);
            (
                if left.is_empty() {
                    None
                } else {
                    Some(RenderNode::Strikethrough(left))
                },
                if right.is_empty() {
                    None
                } else {
                    Some(RenderNode::Strikethrough(right))
                },
            )
        }
        RenderNode::CommentWrapped { children, comment } => {
            let (left, right) = split_children(children, at);
            (
                if left.is_empty() {
                    None
                } else {
                    Some(RenderNode::CommentWrapped {
                        children: left,
                        comment: comment.clone(),
                    })
                },
                if right.is_empty() {
                    None
                } else {
                    Some(RenderNode::CommentWrapped {
                        children: right,
                        comment,
                    })
                },
            )
        }
        RenderNode::HighlightWrapped { children } => {
            let (left, right) = split_children(children, at);
            (
                if left.is_empty() {
                    None
                } else {
                    Some(RenderNode::HighlightWrapped { children: left })
                },
                if right.is_empty() {
                    None
                } else {
                    Some(RenderNode::HighlightWrapped { children: right })
                },
            )
        }
    }
}

fn split_children(children: Vec<RenderNode>, at: usize) -> (Vec<RenderNode>, Vec<RenderNode>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut current_pos = 0;

    for node in children {
        let node_len = get_node_text(&node).len();
        if current_pos + node_len <= at {
            left.push(node);
            current_pos += node_len;
        } else if current_pos >= at {
            right.push(node);
        } else {
            let split_at = at - current_pos;
            let (l, r) = split_node(node, split_at);
            if let Some(l_node) = l {
                left.push(l_node);
            }
            if let Some(r_node) = r {
                right.push(r_node);
            }
            current_pos = at;
        }
    }
    (left, right)
}

fn process_inlines(
    inlines: Vec<MdInline>,
    comments: Option<&Vec<Comment>>,
    pending_highlight: Option<&String>,
) -> Vec<RenderNode> {
    fn convert(inline: MdInline) -> RenderNode {
        match inline {
            MdInline::Text(s) => RenderNode::Text(s),
            MdInline::Code(s) => RenderNode::Code(s),
            MdInline::Link { href, text } => RenderNode::Link { href, text },
            MdInline::Image { src, alt } => RenderNode::Image { src, alt },
            MdInline::SoftBreak => RenderNode::SoftBreak,
            MdInline::HardBreak => RenderNode::HardBreak,
            MdInline::Emphasis(inner) => RenderNode::Emphasis(inner.into_iter().map(convert).collect()),
            MdInline::Strong(inner) => RenderNode::Strong(inner.into_iter().map(convert).collect()),
            MdInline::Strikethrough(inner) => RenderNode::Strikethrough(inner.into_iter().map(convert).collect()),
            MdInline::TaskListMarker(b) => RenderNode::TaskListMarker(b),
        }
    }

    let mut nodes: Vec<RenderNode> = inlines.into_iter().map(convert).collect();

    if let Some(h) = pending_highlight {
        if !h.is_empty() {
            let full_text: String = nodes.iter().map(get_node_text).collect();
            if let Some(start_idx) = full_text.find(h) {
                let end_idx = start_idx + h.len();
                let (before, mid_after) = split_children(nodes, start_idx);
                let (mid, after) = split_children(mid_after, end_idx - start_idx);

                let mut next_nodes = before;
                if !mid.is_empty() {
                    next_nodes.push(RenderNode::HighlightWrapped { children: mid });
                }
                next_nodes.extend(after);
                nodes = next_nodes;
            }
        }
    }

    if let Some(comments) = comments {
        for comment in comments {
            let target_text = &comment.text_selection;
            let full_text: String = nodes.iter().map(get_node_text).collect();

            if let Some(start_idx) = full_text.find(target_text) {
                let end_idx = start_idx + target_text.len();
                let (before, mid_after) = split_children(nodes, start_idx);
                let (mid, after) = split_children(mid_after, end_idx - start_idx);

                let mut next_nodes = before;
                if !mid.is_empty() {
                    next_nodes.push(RenderNode::CommentWrapped {
                        children: mid,
                        comment: comment.clone(),
                    });
                }
                next_nodes.extend(after);
                nodes = next_nodes;
            }
        }
    }
    nodes
}

fn align_class(idx: usize, alignments: &[pulldown_cmark::Alignment]) -> &'static str {
    match alignments.get(idx).unwrap_or(&pulldown_cmark::Alignment::None) {
        pulldown_cmark::Alignment::Left => "text-left",
        pulldown_cmark::Alignment::Center => "text-center",
        pulldown_cmark::Alignment::Right => "text-right",
        pulldown_cmark::Alignment::None => "text-left",
    }
}

#[allow(clippy::only_used_in_recursion)]
fn render_node(
    node: RenderNode,
    comments: Option<&Vec<Comment>>,
    pending_highlight: Option<&String>,
) -> Element {
    match node {
        RenderNode::Text(s) => rsx! { "{s}" },
        RenderNode::Code(s) => rsx! { code { "{s}" } },
        RenderNode::SoftBreak => rsx! { " " },
        RenderNode::HardBreak => rsx! { br {} },
        RenderNode::Link { href, text } => rsx! {
            LinkWithControls { href, text }
        },
        RenderNode::Image { src, alt } => rsx! {
            InlineImage { src, alt }
        },
        RenderNode::Emphasis(children) => rsx! {
            em {
                for child in children {
                    {render_node(child, comments, pending_highlight)}
                }
            }
        },
        RenderNode::Strong(children) => rsx! {
            strong {
                for child in children {
                    {render_node(child, comments, pending_highlight)}
                }
            }
        },
        RenderNode::Strikethrough(children) => rsx! {
            del {
                for child in children {
                    {render_node(child, comments, pending_highlight)}
                }
            }
        },
        RenderNode::TaskListMarker(checked) => rsx! {
            input {
                r#type: "checkbox",
                checked: checked,
                disabled: true,
                class: "mr-2 accent-primary-500 align-middle",
            }
        },
        RenderNode::CommentWrapped { children, comment } => {
            let rendered_children: Vec<Element> = children
                .into_iter()
                .map(|child| render_node(child, comments, pending_highlight))
                .collect();

            rsx! {
                span {
                    class: "border-b-2 border-primary-500 font-bold cursor-pointer relative inline-block group/comment",
                    span {
                        class: "peer",
                        for child_el in rendered_children {
                            {child_el}
                        }
                    }
                    div {
                        class: "absolute top-full left-1/2 transform -translate-x-1/2 pt-2 z-50 opacity-0 pointer-events-none group-hover/comment:opacity-100 group-hover/comment:pointer-events-auto transition-opacity min-w-max",
                        div {
                            class: "bg-app text-fg text-xs rounded shadow-lg px-3 py-2",
                            div {
                                class: "flex flex-col gap-1",
                                div {
                                    class: "whitespace-normal max-w-xs",
                                    "{comment.comment}"
                                }
                                div {
                                    class: "flex justify-end gap-2 mt-1 pt-1 border-t border-faint",
                                    "data-comment-id": "{comment.id}",
                                    span {
                                        class: "p-1 hover:bg-input rounded cursor-pointer text-fg-muted hover:text-fg transition-colors",
                                        title: "Edit comment",
                                        "data-action": "edit",
                                        Icon { width: 12, height: 12, icon: fi_icons::FiEdit2 }
                                    }
                                    span {
                                        class: "p-1 hover:bg-input rounded cursor-pointer text-fg-muted hover:text-red-400 transition-colors",
                                        title: "Delete comment",
                                        "data-action": "delete",
                                        Icon { width: 12, height: 12, icon: fi_icons::FiTrash2 }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        RenderNode::HighlightWrapped { children } => {
            let rendered_children: Vec<Element> = children
                .into_iter()
                .map(|child| render_node(child, comments, pending_highlight))
                .collect();
            rsx! {
                span {
                    class: "bg-yellow-200 dark:bg-yellow-900/40 text-fg",
                    for child_el in rendered_children {
                        {child_el}
                    }
                }
            }
        }
    }
}

fn render_block(
    block: MdBlock,
    comments: Option<&Vec<Comment>>,
    pending_highlight: Option<&String>,
) -> Element {
    match block {
        MdBlock::Header { level, content } => {
            let nodes = process_inlines(content, comments, pending_highlight);
            let inlines = nodes
                .into_iter()
                .map(|i| render_node(i, comments, pending_highlight));
            match level {
                HeadingLevel::H1 => rsx! { h1 { {inlines} } },
                HeadingLevel::H2 => rsx! { h2 { {inlines} } },
                HeadingLevel::H3 => rsx! { h3 { {inlines} } },
                HeadingLevel::H4 => rsx! { h4 { {inlines} } },
                HeadingLevel::H5 => rsx! { h5 { {inlines} } },
                HeadingLevel::H6 => rsx! { h6 { {inlines} } },
            }
        }
        MdBlock::Paragraph(inlines) => rsx! {
            p {
                for node in process_inlines(inlines, comments, pending_highlight) {
                    {render_node(node, comments, pending_highlight)}
                }
            }
        },
        MdBlock::List { items, start } => {
            if let Some(start_num) = start {
                rsx! {
                    ol {
                        start: "{start_num}",
                        for item in items {
                            li {
                                for block in item.blocks {
                                    {render_block(block, comments, pending_highlight)}
                                }
                            }
                        }
                    }
                }
            } else {
                rsx! {
                    ul {
                        for item in items {
                            li {
                                for block in item.blocks {
                                    {render_block(block, comments, pending_highlight)}
                                }
                            }
                        }
                    }
                }
            }
        }
        MdBlock::CodeBlock { lang, code } => rsx! {
            CodeBlock { lang, code }
        },
        MdBlock::Table {
            headers,
            rows,
            alignments,
        } => {
            rsx! {
                div {
                    class: "overflow-x-auto my-4",
                    table {
                        class: "table-auto w-full my-4",
                        thead {
                            class: "bg-section",
                            tr {
                                for (idx, header_cell) in headers.into_iter().enumerate() {
                                    th {
                                        class: "{align_class(idx, &alignments)} font-semibold",
                                        for node in process_inlines(header_cell, comments, pending_highlight) {
                                            {render_node(node, comments, pending_highlight)}
                                        }
                                    }
                                }
                            }
                        }
                        tbody {
                            for row in rows {
                                tr {
                                    class: "border-b border-faint",
                                    for (idx, cell) in row.into_iter().enumerate() {
                                        td {
                                            class: "{align_class(idx, &alignments)}",
                                            for node in process_inlines(cell, comments, pending_highlight) {
                                                {render_node(node, comments, pending_highlight)}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        MdBlock::BlockQuote(child_blocks) => rsx! {
            blockquote {
                class: "border-l-4 border-primary-500 pl-4 my-4 text-fg-muted italic",
                for child_block in child_blocks {
                    {render_block(child_block, comments, pending_highlight)}
                }
            }
        },
        MdBlock::HorizontalRule => rsx! {
            hr { class: "my-6 border-t border-faint" }
        },
        MdBlock::Details { id, summary, body, default_open } => {
            let rendered_body: Vec<Element> = body
                .into_iter()
                .map(|b| render_block(b, comments, pending_highlight))
                .collect();
            rsx! {
                DetailsBlock {
                    id,
                    summary,
                    rendered_body,
                    default_open,
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// DetailsBlock — a Dioxus component so it can use persistent expansion state.
// Body elements are pre-rendered by render_block before being passed here,
// since render_block is a local fn inside MarkdownRenderer and cannot be called
// from a module-level component directly.
// ---------------------------------------------------------------------------
#[component]
fn DetailsBlock(
    id: String,
    summary: String,
    rendered_body: Vec<Element>,
    default_open: bool,
) -> Element {
    let mut expansion_state = use_context::<crate::components::chat::ExpansionStateContext>().0;

    // Use persistent state if available, otherwise fallback to default_open
    let is_open = *expansion_state.read().get(&id).unwrap_or(&default_open);

    rsx! {
        div {
            class: "my-4 border border-faint rounded-lg overflow-hidden",
            // Summary toggle row
            button {
                class: "flex w-full items-center gap-2 px-4 py-3 bg-section hover:bg-input transition-colors text-left cursor-pointer",
                onclick: move |_| {
                    let mut state = expansion_state.write();
                    state.insert(id.clone(), !is_open);
                },
                if is_open {
                    Icon { width: 14, height: 14, icon: fi_icons::FiChevronDown }
                } else {
                    Icon { width: 14, height: 14, icon: fi_icons::FiChevronRight }
                }
                span { class: "text-sm font-semibold text-fg", "{summary}" }
            }
            // Collapsible body — only rendered to DOM when open (Dioxus conditional rendering)
            if is_open {
                div {
                    class: "px-4 py-3 border-t border-faint",
                    for el in rendered_body {
                        {el}
                    }
                }
            }
        }
    }
}


#[component]
pub fn MarkdownRenderer(
    id: Option<uuid::Uuid>,
    content: String,
    comments: Option<Vec<Comment>>,
    pending_highlight: Option<String>,
    #[props(default)] on_comment_edit: Option<EventHandler<(String, f64, f64)>>,
    #[props(default)] on_comment_delete: Option<EventHandler<String>>,
) -> Element {
    // ⚠️  DO NOT WRAP THIS BLOCK IN `use_memo` ⚠️
    //
    // Regression history (v0.9.58, March 2026):
    //   Wrapping the parser in `use_memo(move || { … })` silently broke live
    //   streaming.  In Dioxus 0.6 `use_memo` only re-evaluates when *captured
    //   Signals* change.  `content` is a plain `String` prop — not a Signal —
    //   so the memo captured it by value on first render and never re-ran,
    //   even though `MessageBubble` re-rendered with a longer string on every
    //   streaming chunk.
    //
    // Signal chain that depends on this being inline:
    //   1. StreamMessage::Text arrives
    //   2. MessageBubble's local `content` Signal is appended to (.write())
    //   3. Signal write triggers MessageBubble re-render
    //   4. MessageBubble passes `content: content()` (resolved String) here
    //   5. This block re-parses the updated string → live UI update
    //
    // If you need to optimize, convert the prop to a `ReadOnlySignal<String>`
    // so Dioxus can track it, or throttle at the *caller* side — but do NOT
    // memoize this parse with a String prop.
    let blocks = {
        let content_reader = &content;
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_TABLES);

        let parser = Parser::new_ext(content_reader, options);

        type Block = MdBlock;
        type Inline = MdInline;
        type ListItem = MdListItem;

        let mut blocks: Vec<Block> = Vec::new();
        let mut list_stack: Vec<(Vec<ListItem>, Option<u64>)> = Vec::new();
        let mut list_item_stack: Vec<Vec<Block>> = Vec::new();

        let mut current_inlines: Vec<Inline> = Vec::new();
        let mut inline_stack: Vec<(InlineTag, Vec<Inline>)> = Vec::new();
        let mut current_heading_level: Option<HeadingLevel> = None;

        let mut code_lang = String::new();
        let mut code_buffer = String::new();
        let mut in_code_block = false;

        // State for simplified link handling
        let mut link_href = String::new();
        let mut link_text_buffer = String::new();
        let mut in_link = false;

        // State for image handling (same pattern as links)
        let mut image_src = String::new();
        let mut image_alt_buffer = String::new();
        let mut in_image = false;

        let mut in_table_header = false;
        let mut blockquote_stack: Vec<Vec<Block>> = Vec::new();

        // State for <details>/<summary> HTML tag parsing.
        // pulldown_cmark emits these as raw HtmlBlock events, not structured Tags.
        let mut in_details = false;
        let mut details_open_attr = false;
        let mut details_summary = String::new();
        let mut details_body_blocks: Vec<Block> = Vec::new();
        let mut details_counter = 0;

        let flush_inlines_to_paragraph = |inlines: &mut Vec<Inline>, blocks: &mut Vec<Block>| {
            if !inlines.is_empty() {
                blocks.push(Block::Paragraph(std::mem::take(inlines)));
            }
        };

        // --- Parser Logic ---
        for event in parser {
            // Block routing priority:
            // 1. Inside a blockquote  → push to blockquote_stack top
            // 2. Inside a list item   → push to list_item_stack top
            // 3. Inside a <details>   → push to details_body_blocks
            // 4. Default              → push to top-level blocks
            let current_blocks = if let Some(bq_blocks) = blockquote_stack.last_mut() {
                bq_blocks
            } else if let Some(item_blocks) = list_item_stack.last_mut() {
                item_blocks
            } else if in_details {
                &mut details_body_blocks
            } else {
                &mut blocks
            };

            match event {
                Event::Start(Tag::Table(aligns)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    let alignments: Vec<Alignment> = aligns.into_iter().collect();
                    current_blocks.push(Block::Table {
                        headers: Vec::new(),
                        rows: Vec::new(),
                        alignments,
                    });
                }
                Event::Start(Tag::TableHead) => {
                    in_table_header = true;
                }
                Event::Start(Tag::TableRow) => {
                    if !in_table_header {
                        if let Some(Block::Table { rows, .. }) = current_blocks.last_mut() {
                            rows.push(Vec::new());
                        }
                    }
                }
                Event::Start(Tag::TableCell) => {
                    current_inlines.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    if let Some(Block::Table { headers, rows, .. }) = current_blocks.last_mut() {
                        if in_table_header {
                            headers.push(std::mem::take(&mut current_inlines));
                        } else {
                            if let Some(last_row) = rows.last_mut() {
                                last_row.push(std::mem::take(&mut current_inlines));
                            }
                        }
                    }
                }
                Event::End(TagEnd::TableRow) => {
                    // Handled by cell logic
                }
                Event::End(TagEnd::TableHead) => {
                    in_table_header = false;
                }
                Event::End(TagEnd::Table) => {
                    // Handled by cell logic
                }
                Event::Start(Tag::Paragraph) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                }
                Event::End(TagEnd::Paragraph) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    current_heading_level = Some(level);
                }
                Event::End(TagEnd::Heading(_)) => {
                    if let Some(level) = current_heading_level.take() {
                        current_blocks.push(Block::Header {
                            level,
                            content: std::mem::take(&mut current_inlines),
                        });
                    }
                }
                Event::Start(Tag::List(start)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    list_stack.push((Vec::new(), start));
                }
                Event::End(TagEnd::List(_)) => {
                    if let Some((items, start)) = list_stack.pop() {
                        let target_blocks = if let Some(item_blocks) = list_item_stack.last_mut() {
                            item_blocks
                        } else {
                            &mut blocks
                        };
                        target_blocks.push(Block::List { items, start });
                    }
                }
                Event::Start(Tag::Item) => {
                    list_item_stack.push(Vec::new());
                }
                Event::End(TagEnd::Item) => {
                    if let Some(mut item_blocks) = list_item_stack.pop() {
                        flush_inlines_to_paragraph(&mut current_inlines, &mut item_blocks);
                        if let Some((list, _)) = list_stack.last_mut() {
                            list.push(ListItem {
                                blocks: item_blocks,
                            });
                        }
                    }
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    in_code_block = true;
                    code_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(l) => l.into_string(),
                        _ => String::new(),
                    };
                    code_buffer.clear();
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    current_blocks.push(Block::CodeBlock {
                        lang: code_lang.clone(),
                        code: code_buffer.clone(),
                    });
                }
                Event::Start(Tag::Emphasis) => {
                    inline_stack.push((InlineTag::Emphasis, Vec::new()));
                }
                Event::End(TagEnd::Emphasis) => {
                    if let Some((InlineTag::Emphasis, inlines)) = inline_stack.pop() {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Emphasis(inlines));
                    }
                }
                Event::Start(Tag::Strong) => {
                    inline_stack.push((InlineTag::Strong, Vec::new()));
                }
                Event::End(TagEnd::Strong) => {
                    if let Some((InlineTag::Strong, inlines)) = inline_stack.pop() {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Strong(inlines));
                    }
                }
                Event::Start(Tag::Strikethrough) => {
                    inline_stack.push((InlineTag::Strikethrough, Vec::new()));
                }
                Event::End(TagEnd::Strikethrough) => {
                    if let Some((InlineTag::Strikethrough, inlines)) = inline_stack.pop() {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Strikethrough(inlines));
                    }
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    blockquote_stack.push(Vec::new());
                }
                Event::End(TagEnd::BlockQuote) => {
                    if let Some(bq_blocks) = blockquote_stack.pop() {
                        let target = if let Some(bq_parent) = blockquote_stack.last_mut() {
                            bq_parent
                        } else if let Some(item_blocks) = list_item_stack.last_mut() {
                            item_blocks
                        } else {
                            &mut blocks
                        };
                        target.push(Block::BlockQuote(bq_blocks));
                    }
                }
                Event::Rule => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    current_blocks.push(Block::HorizontalRule);
                }
                Event::TaskListMarker(checked) => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::TaskListMarker(checked));
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    in_link = true;
                    link_href = dest_url.to_string();
                    link_text_buffer.clear();
                }
                Event::End(TagEnd::Link) => {
                    in_link = false;
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::Link {
                        href: link_href.clone(),
                        text: link_text_buffer.clone(),
                    });
                }
                Event::Start(Tag::Image { dest_url, .. }) => {
                    in_image = true;
                    image_src = dest_url.to_string();
                    image_alt_buffer.clear();
                }
                Event::End(TagEnd::Image) => {
                    in_image = false;
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::Image {
                        src: image_src.clone(),
                        alt: image_alt_buffer.clone(),
                    });
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else if in_image {
                        image_alt_buffer.push_str(&text);
                    } else if in_link {
                        link_text_buffer.push_str(&text);
                    } else {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Text(text.to_string()));
                    }
                }
                Event::Code(text) => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::Code(text.to_string()));
                }
                Event::SoftBreak => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::SoftBreak);
                }
                Event::HardBreak => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::HardBreak);
                }
                // Handle raw HTML events for <details>/<summary> blocks.
                // pulldown_cmark emits these as HtmlBlock events, not structured Tags.
                Event::Html(html) | Event::InlineHtml(html) => {
                    let trimmed = html.trim();
                    if trimmed.starts_with("<details") {
                        // Opening tag — flush any pending inlines and start details accumulation
                        flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                        in_details = true;
                        details_open_attr = trimmed.contains(" open\"")
                            || trimmed.contains(" open>")
                            || trimmed.contains(" open ")
                            || trimmed.ends_with(" open");
                        details_summary.clear();
                        details_body_blocks.clear();
                    } else if in_details && trimmed.starts_with("<summary>") {
                        // Summary line: extract the text between <summary>...</summary>
                        // Handle both inline and multi-line by stripping known wrapper tags
                        let inner = trimmed
                            .trim_start_matches("<summary>")
                            .trim_end_matches("</summary>")
                            .trim();
                        // Strip any remaining HTML tags from the summary via ammonia
                        let sanitized = ammonia::Builder::new()
                            .tags(std::collections::HashSet::new())
                            .clean(inner)
                            .to_string();
                        details_summary = sanitized;
                    } else if trimmed == "</details>" && in_details {
                        // Closing tag — flush body inlines and push the completed Details block
                        in_details = false;
                        flush_inlines_to_paragraph(&mut current_inlines, &mut details_body_blocks);
                        // Push to the correct parent scope (not details_body_blocks — that's done)
                        let target = if let Some(bq_blocks) = blockquote_stack.last_mut() {
                            bq_blocks
                        } else if let Some(item_blocks) = list_item_stack.last_mut() {
                            item_blocks
                        } else {
                            &mut blocks
                        };
                        let block_id = match id {
                            Some(mid) => format!("{mid}:{details_counter}"),
                            None => format!("temp:{details_counter}"),
                        };
                        details_counter += 1;

                        target.push(Block::Details {
                            id: block_id,
                            summary: details_summary.clone(),
                            body: std::mem::take(&mut details_body_blocks),
                            default_open: details_open_attr,
                        });
                    }
                    // All other raw HTML is intentionally discarded (existing behaviour)
                }
                _ => {}
            }
        }
        // Safety flush: if a <details> was opened but never closed (truncated stream),
        // recover accumulated content rather than silently dropping it.
        if in_details {
            flush_inlines_to_paragraph(&mut current_inlines, &mut details_body_blocks);
            let block_id = match id {
                Some(mid) => format!("{mid}:{details_counter}"),
                None => format!("temp:{details_counter}"),
            };
            blocks.push(Block::Details {
                id: block_id,
                summary: if details_summary.is_empty() {
                    "Details".to_string()
                } else {
                    details_summary.clone()
                },
                body: std::mem::take(&mut details_body_blocks),
                default_open: true, // Show partial content by default
            });
        } else {
            flush_inlines_to_paragraph(&mut current_inlines, &mut blocks);
        }

        blocks
    };

    let elements = {
        let comments_ref = comments.as_ref();
        let pending_highlight_ref = pending_highlight.as_ref();

        blocks
            .iter()
            .cloned()
            .map(|b| render_block(b, comments_ref, pending_highlight_ref))
            .collect::<Vec<_>>()
    };

    // Generate a unique ID for this renderer instance
    let container_id = use_signal(|| {
        format!(
            "markdown-renderer-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("x")
        )
    });

    // Set up event delegation for comment actions
    use_effect(move || {
        let id = container_id();
        let on_edit = on_comment_edit;
        let on_delete = on_comment_delete;

        spawn(async move {
            let mut eval = document::eval(&format!(
                r#"
                const container = document.getElementById('{}');
                if (container) {{
                    container.addEventListener('click', (e) => {{
                        const target = e.target.closest('[data-action]');
                        if (target) {{
                            const action = target.getAttribute('data-action');
                            const commentIdEl = target.closest('[data-comment-id]');
                            const commentId = commentIdEl ? commentIdEl.getAttribute('data-comment-id') : null;
                            if (commentId) {{
                                // Anchor the edit popover to the comment span, using the
                                // same prefer-below/flip-above logic as the selection handler.
                                const rect = commentIdEl.getBoundingClientRect();
                                const popoverHeight = 160;
                                const popoverWidth = 384; // max-w-[24rem]
                                const spaceBelow = window.innerHeight - rect.bottom;
                                let top;
                                if (spaceBelow < popoverHeight + 20) {{
                                    top = rect.top + window.scrollY - popoverHeight - 8;
                                }} else {{
                                    top = rect.bottom + window.scrollY + 8;
                                }}
                                let left = rect.left + window.scrollX;
                                left = Math.max(8, Math.min(left, window.innerWidth - popoverWidth - 8));
                                dioxus.send({{ action: action, comment_id: commentId, top: top, left: left }});
                            }}
                        }}
                    }});
                }}
            "#,
                id
            ));

            #[derive(serde::Deserialize)]
            struct CommentAction {
                action: String,
                comment_id: String,
                #[serde(default)]
                top: f64,
                #[serde(default)]
                left: f64,
            }

            while let Ok(msg) = eval.recv().await {
                if let Ok(action) = serde_json::from_value::<CommentAction>(msg) {
                    match action.action.as_str() {
                        "edit" => {
                            if let Some(handler) = &on_edit {
                                handler.call((action.comment_id, action.top, action.left));
                            }
                        }
                        "delete" => {
                            if let Some(handler) = &on_delete {
                                handler.call(action.comment_id);
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    });

    rsx! {
        div {
            id: "{container_id}",
            class: "prose prose-sm dark:prose-invert w-full max-w-full break-words",
            style: "word-wrap: break-word; overflow-wrap: anywhere;",
            for el in elements.iter() {
                {el.clone()}
            }
        }
    }
}

/// A lightweight markdown renderer for thinking/reasoning content.
///
/// - `compact: true` - Inline formatting only, no paragraph spacing (for streaming bubble)
/// - `compact: false` - Full markdown rendering (for Thinking Process section)
#[component]
pub fn ThinkingMarkdownRenderer(
    content: String,
    #[props(default = false)] compact: bool,
) -> Element {
    let elements = {
        let content_reader = &content;
        let mut options = pulldown_cmark::Options::empty();
        options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);

        let parser = pulldown_cmark::Parser::new_ext(content_reader, options);

        // Simplified IR for thinking content
        #[derive(Debug, Clone)]
        enum Inline {
            Text(String),
            Code(String),
            SoftBreak,
            HardBreak,
            Emphasis(Vec<Inline>),
            Strong(Vec<Inline>),
        }

        #[derive(Debug, Clone)]
        enum Block {
            Paragraph(Vec<Inline>),
            CodeBlock { lang: String, code: String },
        }

        #[derive(Debug, Clone, PartialEq)]
        enum InlineTag {
            Emphasis,
            Strong,
        }

        let mut blocks: Vec<Block> = Vec::new();
        let mut current_inlines: Vec<Inline> = Vec::new();
        let mut inline_stack: Vec<(InlineTag, Vec<Inline>)> = Vec::new();

        let mut code_lang = String::new();
        let mut code_buffer = String::new();
        let mut in_code_block = false;

        let flush_inlines = |inlines: &mut Vec<Inline>, blocks: &mut Vec<Block>| {
            if !inlines.is_empty() {
                blocks.push(Block::Paragraph(std::mem::take(inlines)));
            }
        };


        for event in parser {
            match event {
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Paragraph) => {
                    flush_inlines(&mut current_inlines, &mut blocks);
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Paragraph) => {
                    flush_inlines(&mut current_inlines, &mut blocks);
                }
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(kind)) => {
                    flush_inlines(&mut current_inlines, &mut blocks);
                    in_code_block = true;
                    code_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(l) => l.into_string(),
                        _ => String::new(),
                    };
                    code_buffer.clear();
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                    in_code_block = false;
                    blocks.push(Block::CodeBlock {
                        lang: code_lang.clone(),
                        code: code_buffer.clone(),
                    });
                }
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Emphasis) => {
                    inline_stack.push((InlineTag::Emphasis, Vec::new()));
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Emphasis) => {
                    if let Some((InlineTag::Emphasis, inlines)) = inline_stack.pop() {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Emphasis(inlines));
                    }
                }
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Strong) => {
                    inline_stack.push((InlineTag::Strong, Vec::new()));
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Strong) => {
                    if let Some((InlineTag::Strong, inlines)) = inline_stack.pop() {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Strong(inlines));
                    }
                }
                pulldown_cmark::Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else {
                        let target = active_target!(inline_stack, current_inlines);
                        target.push(Inline::Text(text.to_string()));
                    }
                }
                pulldown_cmark::Event::Code(text) => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::Code(text.to_string()));
                }
                pulldown_cmark::Event::SoftBreak => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::SoftBreak);
                }
                pulldown_cmark::Event::HardBreak => {
                    let target = active_target!(inline_stack, current_inlines);
                    target.push(Inline::HardBreak);
                }
                _ => {}
            }
        }
        flush_inlines(&mut current_inlines, &mut blocks);

        // Render functions
        fn render_inline(inline: Inline) -> Element {
            match inline {
                Inline::Text(text) => rsx! { span { "{text}" } },
                Inline::Code(text) => rsx! {
                    code {
                        class: "bg-section text-gray-200 font-mono rounded px-1",
                        "{text}"
                    }
                },
                Inline::SoftBreak => rsx! { br {} },
                Inline::HardBreak => rsx! { br {} },
                Inline::Emphasis(children) => rsx! {
                    em {
                        for child in children { {render_inline(child)} }
                    }
                },
                Inline::Strong(children) => rsx! {
                    strong {
                        for child in children { {render_inline(child)} }
                    }
                },
            }
        }

        fn render_block(block: Block, compact: bool) -> Element {
            match block {
                Block::Paragraph(inlines) => {
                    if compact {
                        // Compact: inline span with trailing line break
                        rsx! {
                            span {
                                for inline in inlines { {render_inline(inline)} }
                            }
                            br {}
                        }
                    } else {
                        // Full: use proper paragraph with margin for readability
                        rsx! {
                            p {
                                class: "mb-4 leading-relaxed",
                                for inline in inlines { {render_inline(inline)} }
                            }
                        }
                    }
                }
                Block::CodeBlock { lang, code } => {
                    if compact {
                        // Compact: simple inline code styling
                        rsx! {
                            code {
                                class: "bg-section text-gray-200 font-mono text-xs px-1 rounded",
                                "{code}"
                            }
                        }
                    } else {
                        // Full: use proper CodeBlock component
                        rsx! {
                            CodeBlock { lang: lang, code: code }
                        }
                    }
                }
            }
        }

        blocks
            .into_iter()
            .map(|b| render_block(b, compact))
            .collect::<Vec<_>>()
    };

    if compact {
        rsx! {
            span {
                class: "thinking-content-compact",
                for el in elements.iter() { {el.clone()} }
            }
        }
    } else {
        rsx! {
            div {
                class: "thinking-content-full text-fg-muted",
                for el in elements.iter() { {el.clone()} }
            }
        }
    }
}

/// Inline image display with download controls.
/// Extracted as a component so `use_signal` can drive button state feedback.
#[component]
fn InlineImage(src: String, alt: String) -> Element {
    let mut saved = use_signal(|| false);
    let is_local = src.starts_with("file://") || src.starts_with("/");
    let src_for_download = src.clone();

    // Convert file:// paths to data URIs for reliable WebView rendering
    let display_src = if src.starts_with("file://") {
        // Security: validate the resolved path is inside a known safe directory.
        // Uses the shared utility in crate::security to prevent arbitrary file read
        // via malicious model-injected markdown image tags.
        if let Some(safe_path) = crate::security::validate_safe_file_path(&src) {
            match std::fs::read(&safe_path) {
                Ok(bytes) => {
                    let mime = crate::security::mime_from_extension(
                        safe_path.to_str().unwrap_or("")
                    );
                    let b64 =
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
                    format!("data:{};base64,{}", mime, b64)
                }
                Err(e) => {
                    tracing::warn!("Failed to read image at {}: {}", safe_path.display(), e);
                    src.clone()
                }
            }
        } else {
            src.clone() // Fall back to raw src (won't render, but won't leak data)
        }
    } else {
        src.clone()
    };

    rsx! {
        div {
            class: "my-3 inline-block max-w-full",
            img {
                src: "{display_src}",
                alt: "{alt}",
                class: "max-w-full rounded-lg shadow-md max-h-96 object-contain",
            }
            if is_local {
                button {
                    class: if *saved.read() {
                        "mt-2 flex items-center gap-1 px-3 py-1.5 text-xs font-medium rounded-md bg-green-600 text-white transition-colors"
                    } else {
                        "mt-2 flex items-center gap-1 px-3 py-1.5 text-xs font-medium rounded-md bg-primary-600 hover:bg-primary-500 text-white transition-colors"
                    },
                    onclick: move |_| {
                        let file_path = src_for_download.clone();
                        
                        // Security gate: validate path before allowing copy
                        if let Some(safe_source) = crate::security::validate_safe_file_path(&file_path) {
                            if let Some(file_name) = safe_source.file_name() {
                                let downloads = dirs::download_dir().unwrap_or_else(|| {
                                    dirs::home_dir().unwrap_or_default().join("Downloads")
                                });
                                let dest = downloads.join(file_name);
                                match std::fs::copy(&safe_source, &dest) {
                                    Ok(_) => {
                                        tracing::info!("Image saved to {:?}", dest);
                                        saved.set(true);
                                        spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                            saved.set(false);
                                        });
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to save image: {}", e);
                                    }
                                }
                            }
                        } else {
                            tracing::warn!("Blocked attempt to download unsafe file path: {}", file_path);
                        }
                    },
                    if *saved.read() {
                        Icon { width: 14, height: 14, icon: fi_icons::FiCheck }
                        "Saved!"
                    } else {
                        Icon { width: 14, height: 14, icon: fi_icons::FiDownload }
                        "Save to Downloads"
                    }
                }
            }
        }
    }
}
