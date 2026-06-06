use std::sync::LazyLock;

const UI_TEMPLATE: &str = include_str!("../inspector.html");
const UI_CSS: &str = include_str!("../inspector.css");
const UI_JS: &str = include_str!("../inspector.js");

pub static UI_HTML: LazyLock<String> = LazyLock::new(|| {
    UI_TEMPLATE
        .replace("__ONAIR_INSPECTOR_CSS__", UI_CSS)
        .replace("__ONAIR_INSPECTOR_JS__", UI_JS)
});

pub fn ui_html() -> &'static str {
    UI_HTML.as_str()
}
