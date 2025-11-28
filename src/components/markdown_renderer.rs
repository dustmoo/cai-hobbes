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

        fn render_inline(inline: Inline, comments: Option<&Vec<Comment>>, pending_highlight: Option<&String>) -> Element {
            match inline {
                Inline::Text(text) => {
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

                    // 2. Check for comments in this text block
                    if let Some(comments_list) = comments {
                        for comment in comments_list {
                            if text.contains(&comment.text_selection) {
                                let parts: Vec<&str> = text.split(&comment.text_selection).collect();
                                if parts.len() >= 2 {
                                    // Found a match! Render with highlight
                                    // Note: This simple split only handles one occurrence and one comment per text block for now
                                    // to avoid complex recursion in this iteration.
                                    return rsx! {
                                        span {
                                            span { dangerous_inner_html: "{parts[0]}" }
                                            span {
                                                class: "border-b-2 border-primary-500 font-bold cursor-pointer group relative",
                                                "{comment.text_selection}"
                                                // Tooltip
                                                div {
                                                    class: "absolute bottom-full left-1/2 transform -translate-x-1/2 mb-2 px-3 py-2 bg-gray-900 text-white text-xs rounded shadow-lg opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none whitespace-nowrap z-10",
                                                    "{comment.comment}"
                                                }
                                            }
                                            span { dangerous_inner_html: "{parts[1]}" }
                                        }
                                    };
                                }
                            }
                        }
                    }
                    rsx!{ span { dangerous_inner_html: "{text}" } }
                },
                Inline::Code(text) => rsx! {
                    code {
                        class: "bg-gray-800 text-gray-200 font-mono rounded-md px-2 py-1",
                        "{text}"
                    }
                },
                Inline::Link { href, text } => rsx!{ LinkWithControls { href: href, text: text } },
                Inline::SoftBreak => rsx!{ " " },
                Inline::HardBreak => rsx!{ br {} },
                Inline::Emphasis(children) => rsx! {
                    em {
                        for child in children {
                            {render_inline(child, comments, pending_highlight)}
                        }
                    }
                },
                Inline::Strong(children) => rsx! {
                    strong {
                        for child in children {
                            {render_inline(child, comments, pending_highlight)}
                        }
                    }
                },
            }
        }

        fn render_block(block: Block, comments: Option<&Vec<Comment>>, pending_highlight: Option<&String>) -> Element {
            match block {
                Block::Header { level, content } => {
                    let inlines = content.into_iter().map(|i| render_inline(i, comments, pending_highlight));
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
                        for inline in inlines {
                            {render_inline(inline, comments, pending_highlight)}
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
                                          for inline in header_cell {
                                              {render_inline(inline, comments, pending_highlight)}
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
                                              for inline in cell {
                                                  {render_inline(inline, comments, pending_highlight)}
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