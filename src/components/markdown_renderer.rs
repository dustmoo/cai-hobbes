use crate::components::chat::{CodeBlock, Comment, LinkWithControls};
use ammonia::clean;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};
use pulldown_cmark::{html, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[component]
pub fn MarkdownRenderer(
    content: String,
    comments: Option<Vec<Comment>>,
    pending_highlight: Option<String>,
    #[props(default)] on_comment_edit: Option<EventHandler<String>>,
    #[props(default)] on_comment_delete: Option<EventHandler<String>>,
) -> Element {
    let elements = {
        let content_reader = &content;
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_TABLES);

        let parser = Parser::new_ext(content_reader, options);

        // --- Intermediate Representation (IR) ---
        #[derive(Debug, Clone)]
        #[allow(clippy::enum_variant_names)]
        enum Block {
            Header {
                level: HeadingLevel,
                content: Vec<Inline>,
            },
            Paragraph(Vec<Inline>),
            List {
                items: Vec<ListItem>,
                start: Option<u64>,
            },
            CodeBlock {
                lang: String,
                code: String,
            },
            Table {
                headers: Vec<Vec<Inline>>,
                rows: Vec<Vec<Vec<Inline>>>,
            },
        }

        #[derive(Debug, Clone)]
        enum Inline {
            Text(String),
            Code(String),
            Link { href: String, text: String },
            SoftBreak,
            HardBreak,
            Emphasis(Vec<Inline>),
            Strong(Vec<Inline>),
        }

        #[derive(Debug, Clone)]
        struct ListItem {
            blocks: Vec<Block>,
        }

        // --- Parser State ---
        #[derive(Debug, Clone, PartialEq)]
        enum InlineTag {
            Emphasis,
            Strong,
        }

        #[derive(Debug, Clone)]
        enum RenderNode {
            Text(String),
            Code(String),
            Link {
                href: String,
                text: String,
            },
            SoftBreak,
            HardBreak,
            Emphasis(Vec<RenderNode>),
            Strong(Vec<RenderNode>),
            CommentWrapped {
                children: Vec<RenderNode>,
                comment: Comment,
            },
        }

        fn get_node_text(node: &RenderNode) -> String {
            match node {
                RenderNode::Text(s) | RenderNode::Code(s) => s.clone(),
                RenderNode::Link { text, .. } => text.clone(),
                RenderNode::SoftBreak => " ".to_string(),
                RenderNode::HardBreak => "\n".to_string(),
                RenderNode::Emphasis(children)
                | RenderNode::Strong(children)
                | RenderNode::CommentWrapped { children, .. } => {
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
                RenderNode::SoftBreak | RenderNode::HardBreak => {
                    // Should be covered by at==0 or at>=len checks (len is 1)
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
            }
        }

        fn split_children(
            children: Vec<RenderNode>,
            at: usize,
        ) -> (Vec<RenderNode>, Vec<RenderNode>) {
            let mut left = Vec::new();
            let mut right = Vec::new();
            let mut current_len = 0;
            let mut split_done = false;

            for child in children {
                if split_done {
                    right.push(child);
                    continue;
                }

                let child_len = get_node_text(&child).len();
                if current_len + child_len <= at {
                    left.push(child);
                    current_len += child_len;
                } else {
                    // Split this child
                    let split_at = at - current_len;
                    let (l, r) = split_node(child, split_at);
                    if let Some(l_node) = l {
                        left.push(l_node);
                    }
                    if let Some(r_node) = r {
                        right.push(r_node);
                    }
                    current_len += child_len; // Effectively
                    split_done = true;
                }
            }
            (left, right)
        }

        fn process_inlines(
            inlines: Vec<Inline>,
            comments: Option<&Vec<Comment>>,
        ) -> Vec<RenderNode> {
            // 1. Initial conversion
            let nodes: Vec<RenderNode> = inlines
                .into_iter()
                .map(|inline| match inline {
                    Inline::Text(t) => RenderNode::Text(t),
                    Inline::Code(c) => RenderNode::Code(c),
                    Inline::Link { href, text } => {
                        let lower = href.to_lowercase();
                        // Security: Prevent XSS via javascript: and vbscript: protocols
                        // Also maintain caution with data: URIs in links (though less critical than scripts)
                        let safe_href = if lower.starts_with("javascript:")
                            || lower.starts_with("vbscript:")
                            || (lower.starts_with("data:") && !lower.starts_with("data:image/"))
                        {
                            format!("unsafe:{}", href)
                        } else {
                            href
                        };
                        RenderNode::Link {
                            href: safe_href,
                            text,
                        }
                    }
                    Inline::SoftBreak => RenderNode::SoftBreak,
                    Inline::HardBreak => RenderNode::HardBreak,
                    Inline::Emphasis(children) => {
                        RenderNode::Emphasis(process_inlines(children, comments))
                    }
                    Inline::Strong(children) => {
                        RenderNode::Strong(process_inlines(children, comments))
                    }
                })
                .collect();

            if let Some(comments_list) = comments {
                // 2. Build full text
                let full_text: String = nodes.iter().map(get_node_text).collect();

                // 3. Find matches
                let mut matches: Vec<(usize, usize, Comment)> = Vec::new();
                for comment in comments_list {
                    let selection = &comment.text_selection;
                    if selection.is_empty() {
                        continue;
                    }

                    for (start, _) in full_text.match_indices(selection) {
                        let end = start + selection.len();
                        matches.push((start, end, comment.clone()));
                    }
                }

                // Sort matches by start position
                matches.sort_by_key(|(start, _, _)| *start);

                // Filter overlaps (greedy: take first that fits)
                let mut filtered_matches = Vec::new();
                let mut last_end = 0;
                for (start, end, comment) in matches {
                    if start >= last_end {
                        filtered_matches.push((start, end, comment));
                        last_end = end;
                    }
                }

                if filtered_matches.is_empty() {
                    return nodes;
                }

                // 4. Apply matches
                let mut new_nodes = Vec::new();
                let mut current_offset = 0;
                let mut nodes_iter = nodes.into_iter();
                let mut current_node: Option<RenderNode> = nodes_iter.next();

                for (match_start, match_end, comment) in filtered_matches {
                    // a) Consume nodes until match_start
                    while let Some(node) = current_node {
                        let node_len = get_node_text(&node).len();
                        if current_offset + node_len <= match_start {
                            // Node is fully before match
                            new_nodes.push(node);
                            current_offset += node_len;
                            current_node = nodes_iter.next();
                        } else {
                            // Node overlaps with match start
                            let split_at = match_start - current_offset;
                            let (pre, post) = split_node(node, split_at);
                            if let Some(p) = pre {
                                new_nodes.push(p);
                            }
                            current_offset += split_at; // Now current_offset == match_start
                            current_node = post; // Remaining part of node
                            break;
                        }
                    }

                    // b) Consume nodes until match_end (wrapped)
                    let mut wrapped_children = Vec::new();
                    // current_offset is now match_start

                    while let Some(node) = current_node {
                        let node_len = get_node_text(&node).len();
                        if current_offset + node_len <= match_end {
                            // Node is fully inside match
                            wrapped_children.push(node);
                            current_offset += node_len;
                            current_node = nodes_iter.next();
                            if current_offset == match_end {
                                break;
                            }
                        } else {
                            // Node overlaps with match end
                            let split_at = match_end - current_offset;
                            let (inside, outside) = split_node(node, split_at);
                            if let Some(i) = inside {
                                wrapped_children.push(i);
                            }
                            current_offset += split_at; // Now current_offset == match_end
                            current_node = outside;
                            break;
                        }
                    }

                    new_nodes.push(RenderNode::CommentWrapped {
                        children: wrapped_children,
                        comment,
                    });
                }

                // c) Consume remaining nodes
                if let Some(node) = current_node {
                    new_nodes.push(node);
                }
                new_nodes.extend(nodes_iter);

                return new_nodes;
            }

            nodes
        }

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

        let mut in_table_header = false;

        let flush_inlines_to_paragraph = |inlines: &mut Vec<Inline>, blocks: &mut Vec<Block>| {
            if !inlines.is_empty() {
                blocks.push(Block::Paragraph(std::mem::take(inlines)));
            }
        };

        // --- Parser Logic ---
        for event in parser {
            let current_blocks = if let Some(item_blocks) = list_item_stack.last_mut() {
                item_blocks
            } else {
                &mut blocks
            };

            match event {
                Event::Start(Tag::Table(_)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    current_blocks.push(Block::Table {
                        headers: Vec::new(),
                        rows: Vec::new(),
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
                        let target = if let Some((_, parent_inlines)) = inline_stack.last_mut() {
                            parent_inlines
                        } else {
                            &mut current_inlines
                        };
                        target.push(Inline::Emphasis(inlines));
                    }
                }
                Event::Start(Tag::Strong) => {
                    inline_stack.push((InlineTag::Strong, Vec::new()));
                }
                Event::End(TagEnd::Strong) => {
                    if let Some((InlineTag::Strong, inlines)) = inline_stack.pop() {
                        let target = if let Some((_, parent_inlines)) = inline_stack.last_mut() {
                            parent_inlines
                        } else {
                            &mut current_inlines
                        };
                        target.push(Inline::Strong(inlines));
                    }
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    in_link = true;
                    link_href = dest_url.to_string();
                    link_text_buffer.clear();
                }
                Event::End(TagEnd::Link) => {
                    in_link = false;
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::Link {
                        href: link_href.clone(),
                        text: link_text_buffer.clone(),
                    });
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else if in_link {
                        link_text_buffer.push_str(&text);
                    } else {
                        let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                            inlines
                        } else {
                            &mut current_inlines
                        };
                        let mut raw_html = String::new();
                        html::push_html(&mut raw_html, std::iter::once(Event::Text(text.clone())));
                        let sanitized = clean(&raw_html);
                        target.push(Inline::Text(sanitized));
                    }
                }
                Event::Code(text) => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::Code(text.to_string()));
                }
                Event::SoftBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::SoftBreak);
                }
                Event::HardBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::HardBreak);
                }
                _ => {}
            }
        }
        flush_inlines_to_paragraph(&mut current_inlines, &mut blocks);

        // --- Renderer Logic ---
        // We need to capture comments to use in render_inline
        let comments_ref = comments.as_ref();
        let pending_highlight_ref = pending_highlight.as_ref();

        #[allow(clippy::only_used_in_recursion)]
        fn render_node(
            node: RenderNode,
            comments: Option<&Vec<Comment>>,
            pending_highlight: Option<&String>,
        ) -> Element {
            match node {
                RenderNode::Text(text) => {
                    // 1. Check for pending highlight (highest priority for visual feedback during selection)
                    if let Some(highlight_text) = pending_highlight {
                        if !highlight_text.is_empty() && text.contains(highlight_text) {
                            let parts: Vec<&str> = text.split(highlight_text).collect();
                            if parts.len() >= 2 {
                                return rsx! {
                                    span {
                                        span { dangerous_inner_html: "{parts[0]}" }
                                        span {
                                            class: "bg-primary-500/30 border-b-2 border-primary-500",
                                            "{highlight_text}"
                                        }
                                        span { dangerous_inner_html: "{parts[1]}" }
                                    }
                                };
                            }
                        }
                    }
                    rsx! { span { dangerous_inner_html: "{text}" } }
                }
                RenderNode::Code(text) => rsx! {
                    code {
                        class: "bg-gray-800 text-gray-200 font-mono rounded-md px-2 py-1",
                        "{text}"
                    }
                },
                RenderNode::Link { href, text } => {
                    rsx! { LinkWithControls { href: href, text: text } }
                }
                RenderNode::SoftBreak => rsx! { " " },
                RenderNode::HardBreak => rsx! { br {} },
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
                RenderNode::CommentWrapped { children, comment } => {
                    // Render children to Elements first
                    let rendered_children: Vec<Element> = children
                        .into_iter()
                        .map(|child| render_node(child, comments, pending_highlight))
                        .collect();

                    rsx! {
                        span {
                            class: "border-b-2 border-primary-500 font-bold cursor-pointer relative inline-block group/comment",
                            // Highlighted content
                            span {
                                class: "peer",
                                for child_el in rendered_children {
                                    {child_el}
                                }
                            }
                            // Tooltip - hidden and non-interactive by default
                            // Becomes visible and interactive only when the group (parent) is hovered
                            div {
                                class: "absolute top-full left-1/2 transform -translate-x-1/2 pt-2 z-50 opacity-0 pointer-events-none group-hover/comment:opacity-100 group-hover/comment:pointer-events-auto transition-opacity min-w-max",
                                div {
                                    class: "bg-gray-900 text-white text-xs rounded shadow-lg px-3 py-2",
                                    div {
                                        class: "flex flex-col gap-1",
                                        // Comment text
                                        div {
                                            class: "whitespace-normal max-w-xs",
                                            "{comment.comment}"
                                        }
                                        // Controls row
                                        div {
                                            class: "flex justify-end gap-2 mt-1 pt-1 border-t border-gray-700",
                                            "data-comment-id": "{comment.id}",
                                            span {
                                                class: "p-1 hover:bg-gray-700 rounded cursor-pointer text-gray-400 hover:text-white transition-colors",
                                                title: "Edit comment",
                                                "data-action": "edit",
                                                Icon { width: 12, height: 12, icon: fi_icons::FiEdit2 }
                                            }
                                            span {
                                                class: "p-1 hover:bg-gray-700 rounded cursor-pointer text-gray-400 hover:text-red-400 transition-colors",
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
            }
        }

        fn render_block(
            block: Block,
            comments: Option<&Vec<Comment>>,
            pending_highlight: Option<&String>,
        ) -> Element {
            match block {
                Block::Header { level, content } => {
                    let nodes = process_inlines(content, comments);
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
                Block::Paragraph(inlines) => rsx! {
                    p {
                        for node in process_inlines(inlines, comments) {
                            {render_node(node, comments, pending_highlight)}
                        }
                    }
                },
                Block::List { items, start } => {
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
                Block::CodeBlock { lang, code } => rsx! {
                    CodeBlock { lang: lang, code: code }
                },
                Block::Table { headers, rows } => rsx! {
                    div {
                        class: "overflow-x-auto",
                        table {
                            class: "table-auto w-full my-4",
                            thead {
                                class: "bg-gray-800",
                                tr {
                                    for header_cell in headers {
                                        th {
                                            class: "px-4 py-2 text-left font-semibold",
                                            for node in process_inlines(header_cell, comments) {
                                                {render_node(node, comments, pending_highlight)}
                                            }
                                        }
                                    }
                                }
                            }
                            tbody {
                                for row in rows {
                                    tr {
                                        class: "border-b border-gray-700",
                                        for cell in row {
                                            td {
                                                class: "px-4 py-2",
                                                for node in process_inlines(cell, comments) {
                                                    {render_node(node, comments, pending_highlight)}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }

        blocks
            .into_iter()
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
                                dioxus.send({{ action: action, comment_id: commentId }});
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
            }

            while let Ok(msg) = eval.recv().await {
                if let Ok(action) = serde_json::from_value::<CommentAction>(msg) {
                    match action.action.as_str() {
                        "edit" => {
                            if let Some(handler) = &on_edit {
                                handler.call(action.comment_id);
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
                        let target = if let Some((_, parent)) = inline_stack.last_mut() {
                            parent
                        } else {
                            &mut current_inlines
                        };
                        target.push(Inline::Emphasis(inlines));
                    }
                }
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Strong) => {
                    inline_stack.push((InlineTag::Strong, Vec::new()));
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Strong) => {
                    if let Some((InlineTag::Strong, inlines)) = inline_stack.pop() {
                        let target = if let Some((_, parent)) = inline_stack.last_mut() {
                            parent
                        } else {
                            &mut current_inlines
                        };
                        target.push(Inline::Strong(inlines));
                    }
                }
                pulldown_cmark::Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else {
                        let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                            inlines
                        } else {
                            &mut current_inlines
                        };
                        let mut raw_html = String::new();
                        pulldown_cmark::html::push_html(
                            &mut raw_html,
                            std::iter::once(pulldown_cmark::Event::Text(text.clone())),
                        );
                        let sanitized = clean(&raw_html);
                        target.push(Inline::Text(sanitized));
                    }
                }
                pulldown_cmark::Event::Code(text) => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::Code(text.to_string()));
                }
                pulldown_cmark::Event::SoftBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::SoftBreak);
                }
                pulldown_cmark::Event::HardBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() {
                        inlines
                    } else {
                        &mut current_inlines
                    };
                    target.push(Inline::HardBreak);
                }
                _ => {}
            }
        }
        flush_inlines(&mut current_inlines, &mut blocks);

        // Render functions
        fn render_inline(inline: Inline) -> Element {
            match inline {
                Inline::Text(text) => rsx! { span { dangerous_inner_html: "{text}" } },
                Inline::Code(text) => rsx! {
                    code {
                        class: "bg-gray-800 text-gray-200 font-mono rounded px-1",
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
                                class: "bg-gray-800 text-gray-200 font-mono text-xs px-1 rounded",
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
                class: "thinking-content-full text-gray-300",
                for el in elements.iter() { {el.clone()} }
            }
        }
    }
}
