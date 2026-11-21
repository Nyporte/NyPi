//! HTML from text that cannot carry script: escaping, and Markdown with raw HTML shown as text
//! and only safe link targets, for the HTML export (decision D53). The board page applies the same rules.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};

/// Markdown as HTML that cannot carry script: raw HTML in the text is shown as text, and links
/// and images keep only web, mail, and in-page targets.
pub fn markdown(text: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(text, options).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Link { link_type, dest_url: safe_url(dest_url), title, id })
        }
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Image { link_type, dest_url: safe_url(dest_url), title, id })
        }
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let lower = url.trim().to_ascii_lowercase();
    let allowed = ["http://", "https://", "mailto:", "#"].iter().any(|p| lower.starts_with(p));
    if allowed { url } else { CowStr::Borrowed("#") }
}

pub fn is_image_type(mime: &str) -> bool {
    mime.strip_prefix("image/").is_some_and(|sub| {
        !sub.is_empty() && sub.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
    })
}

pub fn is_base64(data: &str) -> bool {
    data.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
