use dioxus::prelude::*;
use pulldown_cmark::{html, Options, Parser, Event, Tag, TagEnd, HeadingLevel};
use ammonia::clean;

use crate::components::chat::{CodeBlock, LinkWithControls};

#[component]
pub fn MarkdownRenderer(content: String) -> Element {
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
            List(Vec<ListItem>),
            CodeBlock { lang: String, code: String },
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
        let mut list_stack: Vec<Vec<ListItem>> = Vec::new();
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
                Event::Start(Tag::List(_)) => {
                    flush_inlines_to_paragraph(&mut current_inlines, current_blocks);
                    list_stack.push(Vec::new());
                },
                Event::End(TagEnd::List(_)) => {
                    if let Some(items) = list_stack.pop() {
                        let target_blocks = if let Some(item_blocks) = list_item_stack.last_mut() { item_blocks } else { &mut blocks };
                        target_blocks.push(Block::List(items));
                    }
                },
                Event::Start(Tag::Item) => {
                    list_item_stack.push(Vec::new());
                },
                Event::End(TagEnd::Item) => {
                    if let Some(mut item_blocks) = list_item_stack.pop() {
                        flush_inlines_to_paragraph(&mut current_inlines, &mut item_blocks);
                        if let Some(list) = list_stack.last_mut() {
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
        fn render_inline(inline: Inline) -> Element {
            match inline {
                Inline::Text(text) => rsx!{ span { dangerous_inner_html: "{text}" } },
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
                            {render_inline(child)}
                        }
                    }
                },
                Inline::Strong(children) => rsx! {
                    strong {
                        for child in children {
                            {render_inline(child)}
                        }
                    }
                },
            }
        }

        fn render_block(block: Block) -> Element {
            match block {
                Block::Header { level, content } => {
                    let inlines = content.into_iter().map(render_inline);
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
                            {render_inline(inline)}
                        }
                    }
                },
                Block::List(items) => rsx! {
                    ul {
                        for item in items {
                            li {
                                for block in item.blocks {
                                    {render_block(block)}
                                }
                            }
                        }
                    }
                },
                Block::CodeBlock { lang, code } => rsx! {
                    CodeBlock { lang: lang, code: code }
                },
            }
        }

        blocks.into_iter().map(render_block).collect::<Vec<_>>()
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