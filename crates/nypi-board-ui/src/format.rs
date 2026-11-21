//! How the board says times, sizes, and money, and Markdown that cannot carry script (the same
//! rules as `nypi_core::html::markdown`: raw HTML shown as text, only web and mail links).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};

pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Milliseconds since the epoch, for an RFC 3339 time.
pub fn parse(time: &str) -> Option<f64> {
    let ms = js_sys::Date::parse(time);
    (!time.is_empty() && ms.is_finite()).then_some(ms)
}

/// "12 s", "4 min", "3 h 5 min", "2 d".
pub fn span(ms: f64) -> String {
    let seconds = (ms / 1000.0).max(0.0) as u64;
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        3600..86_400 => {
            let minutes = seconds % 3600 / 60;
            if minutes == 0 { format!("{} h", seconds / 3600) } else { format!("{} h {minutes} min", seconds / 3600) }
        }
        _ => format!("{} d", seconds / 86_400),
    }
}

/// How long ago `time` was, at `clock`.
pub fn ago(time: &str, clock: f64) -> String {
    match parse(time) {
        Some(at) if clock - at < 5000.0 => "just now".into(),
        Some(at) => format!("{} ago", span(clock - at)),
        None => String::new(),
    }
}

/// How long from `time` to `clock`, for things still going on.
pub fn since(time: &str, clock: f64) -> String {
    parse(time).map(|at| span(clock - at)).unwrap_or_default()
}

/// "14:05" in local time.
pub fn clock_time(time: &str) -> String {
    let Some(ms) = parse(time) else { return String::new() };
    let date = js_sys::Date::new(&ms.into());
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}

/// "Today", "Yesterday", or the date, in local time.
pub fn day(time: &str, clock: f64) -> String {
    let Some(ms) = parse(time) else { return String::new() };
    let date = js_sys::Date::new(&ms.into());
    let today = js_sys::Date::new(&clock.into());
    let key = |d: &js_sys::Date| (d.get_full_year(), d.get_month(), d.get_date());
    if key(&date) == key(&today) {
        return "Today".into();
    }
    let yesterday = js_sys::Date::new(&(clock - 86_400_000.0).into());
    if key(&date) == key(&yesterday) {
        return "Yesterday".into();
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {}", date.get_date(), MONTHS[date.get_month() as usize % 12])
}

/// When something will happen: "in 2 h" today, else the day and time.
pub fn when(time: &str, clock: f64) -> String {
    let Some(at) = parse(time) else { return "when its condition holds".into() };
    if at - clock < 60_000.0 {
        return "now".into();
    }
    if at - clock < 12.0 * 3_600_000.0 {
        return format!("in {}", span(at - clock));
    }
    format!("{} {}", day(time, clock), clock_time(time))
}

pub fn tokens(count: u64) -> String {
    match count {
        0..1000 => count.to_string(),
        1000..1_000_000 => format!("{:.1}k", count as f64 / 1000.0),
        _ => format!("{:.2}M", count as f64 / 1_000_000.0),
    }
}

pub fn money(cost: f64) -> String {
    if cost > 0.0 && cost < 0.01 { "<$0.01".into() } else { format!("${cost:.2}") }
}

pub fn seconds(ms: u64) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.0) }
}

/// The last two parts of a path: enough to tell projects apart.
pub fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Markdown as HTML that cannot carry script.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_shows_raw_html_as_text_and_drops_script_links() {
        let html = markdown("<script>x</script>\n\nSee [a](javascript:alert(1)) and [b](https://example.com).");
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>"), "{html}");
        assert!(html.contains("href=\"#\"") && html.contains("href=\"https://example.com\""), "{html}");
    }

    #[test]
    fn spans_and_sizes_read_naturally() {
        assert_eq!(span(42_000.0), "42 s");
        assert_eq!(span(3_900_000.0), "1 h 5 min");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(money(0.004), "<$0.01");
        assert_eq!(short_path("/Users/me/Projects/NyPi/"), "Projects/NyPi");
    }
}
