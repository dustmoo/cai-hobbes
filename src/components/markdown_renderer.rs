use dioxus::prelude::*;
use pulldown_cmark::{html, Options, Parser, Event, Tag, TagEnd, HeadingLevel};
use ammonia::clean;
use crate::components::chat::{CodeBlock, LinkWithControls, Comment};

#[component]
pub fn MarkdownRenderer(content: String, comments: Option<Vec<Comment>>, pending_highlight: Option<String>) -> Element {
    let elements = {
        let content_reader = &content;
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_TABLES);

        let parser = Parser::new_ext(&content_reader, options);

        // --- Intermediate Representation (IR) ---
        #[derive(Debug, Clone)]
        enum Block {
            Header { level: HeadingLevel, content: Vec<Inline> },
            Paragraph(Vec<Inline>),
            List { items: Vec<ListItem>, start: Option<u64> },
            CodeBlock { lang: String, code: String },
            Table { headers: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
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
            Link { href: String, text: String },
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
                RenderNode::Emphasis(children) | RenderNode::Strong(children) | RenderNode::CommentWrapped { children, .. } => {
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
                    (Some(RenderNode::Text(left.to_string())), Some(RenderNode::Text(right.to_string())))
                },
                RenderNode::Code(s) => {
                    let (left, right) = s.split_at(at);
                    (Some(RenderNode::Code(left.to_string())), Some(RenderNode::Code(right.to_string())))
                },
                RenderNode::Link { href, text } => {
                    let (left_text, right_text) = text.split_at(at);
                    (
                        Some(RenderNode::Link { href: href.clone(), text: left_text.to_string() }),
                        Some(RenderNode::Link { href, text: right_text.to_string() })
                    )
                },
                RenderNode::SoftBreak | RenderNode::HardBreak => {
                    // Should be covered by at==0 or at>=len checks (len is 1)
                    (Some(node.clone()), None) 
                },
                RenderNode::Emphasis(children) => {
                    let (left, right) = split_children(children, at);
                    (
                        if left.is_empty() { None } else { Some(RenderNode::Emphasis(left)) },
                        if right.is_empty() { None } else { Some(RenderNode::Emphasis(right)) }
                    )
                },
                RenderNode::Strong(children) => {
                    let (left, right) = split_children(children, at);
                    (
                        if left.is_empty() { None } else { Some(RenderNode::Strong(left)) },
                        if right.is_empty() { None } else { Some(RenderNode::Strong(right)) }
                    )
                },
                RenderNode::CommentWrapped { children, comment } => {
                     let (left, right) = split_children(children, at);
                    (
                        if left.is_empty() { None } else { Some(RenderNode::CommentWrapped { children: left, comment: comment.clone() }) },
                        if right.is_empty() { None } else { Some(RenderNode::CommentWrapped { children: right, comment }) }
                    )
                }
            }
        }

        fn split_children(children: Vec<RenderNode>, at: usize) -> (Vec<RenderNode>, Vec<RenderNode>) {
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
                    if let Some(l_node) = l { left.push(l_node); }
                    if let Some(r_node) = r { right.push(r_node); }
                    current_len += child_len; // Effectively
                    split_done = true;
                }
            }
            (left, right)
        }

        fn process_inlines(inlines: Vec<Inline>, comments: Option<&Vec<Comment>>) -> Vec<RenderNode> {
            // 1. Initial conversion
            let nodes: Vec<RenderNode> = inlines.into_iter().map(|inline| match inline {
                Inline::Text(t) => RenderNode::Text(t),
                Inline::Code(c) => RenderNode::Code(c),
                Inline::Link { href, text } => RenderNode::Link { href, text },
                Inline::SoftBreak => RenderNode::SoftBreak,
                Inline::HardBreak => RenderNode::HardBreak,
                Inline::Emphasis(children) => RenderNode::Emphasis(process_inlines(children, comments)),
                Inline::Strong(children) => RenderNode::Strong(process_inlines(children, comments)),
            }).collect();

            if let Some(comments_list) = comments {
                // 2. Build full text
                let full_text: String = nodes.iter().map(get_node_text).collect();

                // 3. Find matches
                let mut matches: Vec<(usize, usize, Comment)> = Vec::new();
                for comment in comments_list {
                    let selection = &comment.text_selection;
                    if selection.is_empty() { continue; }
                    
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
                        comment: comment,
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
                   current_blocks.push(Block::Table { headers: Vec::new(), rows: Vec::new() });
               },
               Event::Start(Tag::TableHead) => {
                   in_table_header = true;
               },
               Event::Start(Tag::TableRow) => {
                   if !in_table_header {
                       if let Some(Block::Table { rows, .. }) = current_blocks.last_mut() {
                           rows.push(Vec::new());
                       }
                   }
               },
               Event::Start(Tag::TableCell) => {
                   current_inlines.clear();
               },
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
               },
               Event::End(TagEnd::TableRow) => {
                   // Handled by cell logic
               },
               Event::End(TagEnd::TableHead) => {
                   in_table_header = false;
               },
               Event::End(TagEnd::Table) => {
                   // Handled by cell logic
               },
                Event::Start(Tag::Paragraph) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                },
                Event::End(TagEnd::Paragraph) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                },
                Event::Start(Tag::Heading { level, .. }) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    current_heading_level = Some(level);
                },
                Event::End(TagEnd::Heading(_)) => {
                    if let Some(level) = current_heading_level.take() {
                        current_blocks.push(Block::Header { level, content: std::mem::take(&mut current_inlines) });
                    }
                },
                Event::Start(Tag::List(start)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    list_stack.push((Vec::new(), start));
                },
                Event::End(TagEnd::List(_)) => {
                    if let Some((items, start)) = list_stack.pop() {
                        let target_blocks = if let Some(item_blocks) = list_item_stack.last_mut() { item_blocks } else { &mut blocks };
                        target_blocks.push(Block::List { items, start });
                    }
                },
                Event::Start(Tag::Item) => {
                    list_item_stack.push(Vec::new());
                },
                Event::End(TagEnd::Item) => {
                    if let Some(mut item_blocks) = list_item_stack.pop() {
                        flush_inlines_to_paragraph(&mut current_inlines, &mut item_blocks);
                        if let Some((list, _)) = list_stack.last_mut() {
                            list.push(ListItem { blocks: item_blocks });
                        }
                    }
                },
                Event::Start(Tag::CodeBlock(kind)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    in_code_block = true;
                    code_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(l) => l.into_string(),
                        _ => String::new(),
                    };
                    code_buffer.clear();
                },
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    current_blocks.push(Block::CodeBlock { lang: code_lang.clone(), code: code_buffer.clone() });
                },
                Event::Start(Tag::Emphasis) => {
                    inline_stack.push((InlineTag::Emphasis, Vec::new()));
                },
                Event::End(TagEnd::Emphasis) => {
                    if let Some((InlineTag::Emphasis, inlines)) = inline_stack.pop() {
                        let target = if let Some((_, parent_inlines)) = inline_stack.last_mut() { parent_inlines } else { &mut current_inlines };
                        target.push(Inline::Emphasis(inlines));
                    }
                },
                Event::Start(Tag::Strong) => {
                    inline_stack.push((InlineTag::Strong, Vec::new()));
                },
                Event::End(TagEnd::Strong) => {
                    if let Some((InlineTag::Strong, inlines)) = inline_stack.pop() {
                        let target = if let Some((_, parent_inlines)) = inline_stack.last_mut() { parent_inlines } else { &mut current_inlines };
                        target.push(Inline::Strong(inlines));
                    }
                },
                Event::Start(Tag::Link { dest_url, .. }) => {
                    in_link = true;
                    link_href = dest_url.to_string();
                    link_text_buffer.clear();
                },
                Event::End(TagEnd::Link) => {
                    in_link = false;
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() { inlines } else { &mut current_inlines };
                    target.push(Inline::Link { href: link_href.clone(), text: link_text_buffer.clone() });
                },
                Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else if in_link {
                        link_text_buffer.push_str(&text);
                    } else {
                        let target = if let Some((_, inlines)) = inline_stack.last_mut() { inlines } else { &mut current_inlines };
                        let mut raw_html = String::new();
                        html::push_html(&mut raw_html, std::iter::once(Event::Text(text.clone())));
                        let sanitized = clean(&raw_html);
                        target.push(Inline::Text(sanitized));
                    }
                },
                Event::Code(text) => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() { inlines } else { &mut current_inlines };
                    target.push(Inline::Code(text.to_string()));
                },
                Event::SoftBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() { inlines } else { &mut current_inlines };
                    target.push(Inline::SoftBreak);
                },
                Event::HardBreak => {
                    let target = if let Some((_, inlines)) = inline_stack.last_mut() { inlines } else { &mut current_inlines };
                    target.push(Inline::HardBreak);
                },
                _ => {}
            }
        }
        flush_inlines_to_paragraph(&mut current_inlines, &mut blocks);

        // --- Renderer Logic ---
        // We need to capture comments to use in render_inline
        let comments_ref = comments.as_ref();
        let pending_highlight_ref = pending_highlight.as_ref();

        fn render_node(node: RenderNode, comments: Option<&Vec<Comment>>, pending_highlight: Option<&String>) -> Element {
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
                    rsx!{ span { dangerous_inner_html: "{text}" } }
                },
                RenderNode::Code(text) => rsx! {
                    code {
                        class: "bg-gray-800 text-gray-200 font-mono rounded-md px-2 py-1",
                        "{text}"
                    }
                },
                RenderNode::Link { href, text } => rsx!{ LinkWithControls { href: href, text: text } },
                RenderNode::SoftBreak => rsx!{ " " },
                RenderNode::HardBreak => rsx!{ br {} },
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
                    rsx! {
                        span {
                            class: "border-b-2 border-primary-500 font-bold cursor-pointer relative inline-block",
                            // Tooltip
                            div {
                                class: "absolute bottom-full left-1/2 transform -translate-x-1/2 mb-2 px-3 py-2 bg-gray-900 text-white text-xs rounded shadow-lg opacity-0 hover:opacity-100 peer-hover:opacity-100 transition-opacity whitespace-nowrap z-10 pointer-events-auto",
                                "{comment.comment}"
                            }
                            span {
                                class: "peer",
                                for child in children {
                                    {render_node(child, comments, pending_highlight)}
                                }
                            }
                        }
                    }
                }
            }
        }

        fn render_block(block: Block, comments: Option<&Vec<Comment>>, pending_highlight: Option<&String>) -> Element {
            match block {
                Block::Header { level, content } => {
                    let nodes = process_inlines(content, comments);
                    let inlines = nodes.into_iter().map(|i| render_node(i, comments, pending_highlight));
                    match level {
                        HeadingLevel::H1 => rsx!{ h1 { {inlines} } },
                        HeadingLevel::H2 => rsx!{ h2 { {inlines} } },
                        HeadingLevel::H3 => rsx!{ h3 { {inlines} } },
                        HeadingLevel::H4 => rsx!{ h4 { {inlines} } },
                        HeadingLevel::H5 => rsx!{ h5 { {inlines} } },
                        HeadingLevel::H6 => rsx!{ h6 { {inlines} } },
                    }
                },
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
                },
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
              }
           }
       }

       blocks.into_iter().map(|b| render_block(b, comments_ref, pending_highlight_ref)).collect::<Vec<_>>()
    };

    rsx! {
        div {
            class: "prose prose-sm dark:prose-invert max-w-none break-words",
            for el in elements.iter() {
                {el.clone()}
            }
        }
    }
}