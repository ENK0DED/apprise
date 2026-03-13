use std::collections::HashMap;
use once_cell::sync::Lazy;

/// Map common :emoji_name: codes to unicode characters
static EMOJI_MAP: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("smile", "😊");
    m.insert("grinning", "😀");
    m.insert("laughing", "😄");
    m.insert("blush", "😊");
    m.insert("heart", "❤️");
    m.insert("thumbsup", "+1");
    m.insert("+1", "👍");
    m.insert("-1", "👎");
    m.insert("thumbsdown", "👎");
    m.insert("warning", "⚠️");
    m.insert("information_source", "ℹ️");
    m.insert("white_check_mark", "✅");
    m.insert("x", "❌");
    m.insert("red_circle", "🔴");
    m.insert("green_circle", "🟢");
    m.insert("yellow_circle", "🟡");
    m.insert("fire", "🔥");
    m.insert("rocket", "🚀");
    m.insert("star", "⭐");
    m.insert("bell", "🔔");
    m.insert("email", "📧");
    m.insert("envelope", "📧");
    m.insert("phone", "📱");
    m.insert("computer", "💻");
    m.insert("tada", "🎉");
    m.insert("checkered_flag", "🏁");
    m.insert("lock", "🔒");
    m.insert("key", "🔑");
    m.insert("bug", "🐛");
    m.insert("hammer", "🔨");
    m.insert("wrench", "🔧");
    m.insert("gear", "⚙️");
    m.insert("chart", "📊");
    m.insert("chart_with_upwards_trend", "📈");
    m.insert("chart_with_downwards_trend", "📉");
    m.insert("calendar", "📅");
    m.insert("clock1", "🕐");
    m.insert("hourglass", "⌛");
    m.insert("question", "❓");
    m.insert("exclamation", "❗");
    m.insert("alert", "🚨");
    m.insert("no_entry", "⛔");
    m.insert("stop_sign", "🛑");
    m.insert("speaker", "🔊");
    m.insert("mute", "🔇");
    m.insert("mega", "📣");
    m.insert("loudspeaker", "📢");
    m.insert("mailbox", "📪");
    m.insert("incoming_envelope", "📨");
    m.insert("envelope_with_arrow", "📩");
    m.insert("pencil", "✏️");
    m.insert("memo", "📝");
    m.insert("clipboard", "📋");
    m.insert("link", "🔗");
    m.insert("paperclip", "📎");
    m.insert("scissors", "✂️");
    m.insert("wastebasket", "🗑️");
    m.insert("file_folder", "📁");
    m.insert("open_file_folder", "📂");
    m.insert("page_facing_up", "📄");
    m.insert("page_with_curl", "📃");
    m.insert("notebook", "📓");
    m.insert("ledger", "📒");
    m.insert("books", "📚");
    m.insert("book", "📖");
    m.insert("green_book", "📗");
    m.insert("blue_book", "📘");
    m.insert("orange_book", "📙");
    m.insert("moneybag", "💰");
    m.insert("dollar", "💵");
    m.insert("euro", "💶");
    m.insert("credit_card", "💳");
    m.insert("gem", "💎");
    m.insert("chart_bar", "📊");
    m.insert("trophy", "🏆");
    m.insert("medal", "🏅");
    m
});

/// Replace :emoji_name: patterns with unicode characters
pub fn interpret_emojis(text: &str) -> String {
    let re = regex::Regex::new(r":([a-zA-Z0-9_+\-]+):").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let name = &caps[1];
        EMOJI_MAP
            .get(name)
            .map(|s| s.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    })
    .into_owned()
}
