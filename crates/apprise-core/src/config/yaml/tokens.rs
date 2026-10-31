//! Where a YAML setting that names one of a service's URL tokens
//! goes, keyed by schema (upstream `_special_token_handler()`).
//!
//! Generated from Apprise 2.1.0 by
//! `scripts/gen_yaml_tokens.py`; do not edit by hand.

/// What a YAML setting named after a URL token sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlToken {
  /// The URL argument that carries the same value.
  Arg(&'static str),
  /// The URL argument that carries the same list; several settings
  /// that name it add to it.
  List(&'static str),
  /// This part of the URL.
  Part(&'static str),
  /// The place in the URL of this constructor argument, which no URL
  /// argument carries ([`url_position`]).
  At(&'static str),
  /// Several URL values: only the URL can set them.
  UrlOnly,
}

/// One end of a run of path segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
  /// Counted from the first segment.
  Start(usize),
  /// Counted back from the end of the path.
  End(usize),
}

/// What a setting replaces in the URL path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segments {
  /// One segment, the value.
  One,
  /// Segments of a path, the value split on `/`.
  Path,
  /// One segment per value of a list.
  List,
}

/// Where a constructor argument sits in the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
  /// This part of the URL.
  Part(&'static str),
  /// The path segments from `start` to `end` (exclusive).
  Path { start: Bound, end: Bound, kind: Segments },
}

/// Where a constructor argument sits in a URL, and the URL arguments
/// a setting for it overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlPosition {
  pub place: Place,
  pub overrides: &'static [&'static str],
}

/// What the YAML setting `key` of a `schema` service (lowercase) sets,
/// or `None` for a setting applied as the URL argument of its name.
#[rustfmt::skip]
pub fn url_token(schema: &str, key: &str) -> Option<UrlToken> {
  use UrlToken::*;
  Some(match (key, schema) {
    ("access_key_id", "ses" | "sns") => Arg("access"),
    ("access_token", "strmlabs" | "webex" | "wxteams") => At("access_token"),
    ("accesstoken", "hassio" | "hassios") => Arg("token"),
    ("accesstoken", "pbul") => At("accesstoken"),
    ("account_sid", "twilio") => Arg("sid"),
    ("akey", "tweet" | "twitter" | "x") => At("akey"),
    ("api_key", "octopush") => Arg("key"),
    ("api_login", "octopush") => Arg("login"),
    ("api_token", "sinch") => Arg("token"),
    ("apikey", "burstsms" | "gupshup" | "httpsms" | "infobip" | "kaleyra" | "klr" | "nexmo" | "pushy" | "smsmanager" | "smsmgr" | "telnyx" | "vonage") => Arg("key"),
    ("apikey", "brevo" | "dot" | "evolution" | "evolutions" | "fcm" | "flowtriq" | "flowtriqs" | "jira" | "kavenegar" | "mailersend" | "mailgun" | "msgbird" | "notifyre" | "onesignal" | "opsgenie" | "pagerduty" | "postmark" | "prowl" | "sendgrid" | "seven" | "smtp2go" | "sparkpost") => At("apikey"),
    ("apikey", "clicksend" | "telesign") => Part("password"),
    ("app_id", "reddit") => At("app_id"),
    ("app_key", "pushed") => At("app_key"),
    ("app_secret", "reddit") => At("app_secret"),
    ("appuser", "atalk") => At("appuser"),
    ("asecret", "tweet" | "twitter" | "x") => At("asecret"),
    ("auth_id", "plivo") => Arg("id"),
    ("auth_token", "twilio") => Arg("token"),
    ("authkey", "msg91") => At("authkey"),
    ("bot_token", "tgram") => At("bot_token"),
    ("botname", "zulip") => At("botname"),
    ("botname", "discord" | "flock" | "fluxer" | "fluxers" | "guilded" | "ryver" | "slack") => Part("user"),
    ("ckey", "tweet" | "twitter" | "x") => At("ckey"),
    ("client_id", "sendpulse") => Arg("id"),
    ("client_id", "azure" | "o365") => Arg("oauth_id"),
    ("client_id", "kakao" | "kakaotalk" | "ringc") => At("client_id"),
    ("client_secret", "kakao" | "kakaotalk" | "sendpulse") => Arg("secret"),
    ("component", "pagerduty") => At("component"),
    ("csecret", "tweet" | "twitter" | "x") => At("csecret"),
    ("customer_id", "telesign") => Part("user"),
    ("device", "join") => List("to"),
    ("device_id", "dot") => At("device_id"),
    ("device_name", "join") => List("to"),
    ("events", "ifttt") => List("to"),
    ("from_addr", "betterstack" | "groundmail" | "resend") => Arg("from"),
    ("from_email", "ses") => Arg("from"),
    ("from_phone", "46elks" | "bulkvs" | "eight00com" | "elks" | "exotel" | "httpsms" | "nexmo" | "ringc" | "signal" | "signals" | "sinch" | "telnyx" | "twilio" | "voipms" | "vonage") => Arg("from"),
    ("from_phone", "smpp" | "smpps") => At("source"),
    ("from_phone_id", "whatsapp") => Arg("from"),
    ("fullpath", "enigma2" | "enigma2s" | "mmost" | "mmosts") => At("fullpath"),
    ("gateway_id", "threema") => Part("user"),
    ("group", "join") => List("to"),
    ("instance", "evolution" | "evolutions") => At("instance"),
    ("integration_id", "xmatters") => At("integration_id"),
    ("integrationkey", "pagerduty") => At("integrationkey"),
    ("kind", "github") => At("kind"),
    ("language", "whatsapp") => Arg("lang"),
    ("msghook", "notifico" | "notificos") => Arg("token"),
    ("namespace", "pinglet" | "pinglets") => At("namespace"),
    ("number", "github") => At("number"),
    ("organization", "ryver" | "zulip") => At("organization"),
    ("owner", "github") => At("owner"),
    ("path", "gotify" | "gotifys" | "notica" | "noticas" | "pinglet" | "pinglets") => At("fullpath"),
    ("path", "flowtriq" | "flowtriqs") => At("webhook_path"),
    ("privatekey", "psafer" | "psafers") => At("privatekey"),
    ("project", "fcm") => At("project"),
    ("project_id", "notifico" | "notificos") => Arg("project"),
    ("providerkey", "prowl") => At("providerkey"),
    ("refresh_token", "kakao" | "kakaotalk") => At("refresh_token"),
    ("repo", "github") => At("repo"),
    ("secret", "azure" | "o365") => Arg("oauth_secret"),
    ("secret", "signl4") => At("secret"),
    ("secret_access_key", "ses" | "sns") => Arg("secret"),
    ("secret_key", "pjet" | "pjets") => Arg("secret"),
    ("sender", "mobilemessage" | "mobilemsg") => Arg("from"),
    ("sender_id", "burstsms") => Arg("from"),
    ("serial", "blink1") => At("serial"),
    ("serverkey", "kumulos") => At("serverkey"),
    ("service_plan_id", "sinch") => Arg("spi"),
    ("short_code", "sinch" | "twilio") => List("to"),
    ("signature", "workflow" | "workflows") => Arg("sig"),
    ("source", "clickatell" | "cm" | "kavenegar" | "msgbird" | "plivo") => Arg("from"),
    ("source", "pagerduty") => At("source"),
    ("space_id", "sfr") => At("space_id"),
    ("subscriber", "vapid") => Arg("from"),
    ("target", "sfr") => List("to"),
    ("target_bot", "cliq" | "zcliq" | "zohocliq") => List("to"),
    ("target_callsign", "aprs" | "dapnet") => List("to"),
    ("target_channel", "cliq" | "irc" | "ircs" | "mmost" | "mmosts" | "notifiarr" | "pbul" | "pushed" | "revolt" | "rocket" | "rockets" | "signal" | "signals" | "trigv" | "trigvs" | "twist" | "zcliq" | "zohocliq") => List("to"),
    ("target_channel_id", "mmost" | "mmosts" | "twist") => List("to"),
    ("target_channels", "slack" | "xmpp" | "xmpps") => List("to"),
    ("target_chat", "whapi") => List("to"),
    ("target_chat_id", "zalo") => List("to"),
    ("target_contact", "smseagle" | "smseagles") => List("to"),
    ("target_department", "wechat") => List("to"),
    ("target_device", "bark" | "barks" | "fcm" | "gorush" | "gorushs" | "hassio" | "hassios" | "pbul" | "pover" | "psafer" | "psafers" | "pushy") => List("to"),
    ("target_email", "azure" | "brevo" | "deltachat" | "deltachats" | "groundmail" | "mailersend" | "mailgun" | "mailto" | "mailtos" | "o365" | "onesignal" | "pbul" | "pingram" | "postmark" | "psafer" | "psafers" | "resend" | "sendgrid" | "sendpulse" | "slack" | "threema") => List("to"),
    ("target_encoded_id", "slack") => List("to"),
    ("target_escalation", "jira" | "opsgenie") => List("to"),
    ("target_group", "bulksms" | "ncloud" | "nclouds" | "onebot" | "onebots" | "pover" | "serwersms" | "smseagle" | "smseagles" | "whapi" | "whatsapp") => List("to"),
    ("target_id", "kakao" | "kakaotalk" | "pingram") => List("to"),
    ("target_member", "simplepush" | "spush") => List("to"),
    ("target_phone", "smpp" | "smpps") => At("targets"),
    ("target_phone", "46elks" | "atalk" | "bulksms" | "bulkvs" | "burstsms" | "clickatell" | "clicksend" | "cm" | "d7sms" | "eight00com" | "elks" | "evolution" | "evolutions" | "exotel" | "gupshup" | "httpsms" | "infobip" | "kaleyra" | "kavenegar" | "klr" | "mobilemessage" | "mobilemsg" | "msg91" | "msgbird" | "nexmo" | "notifyre" | "octopush" | "plivo" | "ringc" | "serwersms" | "seven" | "signal" | "signals" | "sinch" | "smsc" | "smseagle" | "smseagles" | "smsmanager" | "smsmgr" | "telesign" | "telnyx" | "threema" | "twilio" | "voipms" | "vonage" | "whapi" | "whatsapp") => List("to"),
    ("target_phone_no", "dingtalk" | "sns") => List("to"),
    ("target_player", "onesignal") => List("to"),
    ("target_room", "rocket" | "rockets") => List("to"),
    ("target_room_alias", "matrix" | "matrixs") => List("to"),
    ("target_room_id", "nctalk" | "nctalks") => At("targets"),
    ("target_room_id", "matrix" | "matrixs") => List("to"),
    ("target_schedule", "jira" | "opsgenie") => List("to"),
    ("target_segment", "onesignal") => List("to"),
    ("target_sms", "pingram") => List("to"),
    ("target_stream", "zulip") => List("to"),
    ("target_subreddit", "reddit") => List("to"),
    ("target_tag", "wechat") => List("to"),
    ("target_team", "jira" | "opsgenie") => List("to"),
    ("target_threema_id", "threema") => List("to"),
    ("target_topic", "fcm" | "pushy" | "simplepush" | "sns" | "spush" | "wxpusher") => List("to"),
    ("target_user", "irc" | "ircs" | "jira" | "line" | "mastodon" | "mastodons" | "matrix" | "matrixs" | "ncloud" | "nclouds" | "onebot" | "onebots" | "onesignal" | "opsgenie" | "pushed" | "rocket" | "rockets" | "slack" | "tgram" | "toot" | "toots" | "tweet" | "twitter" | "wechat" | "wxpusher" | "x" | "xmpp" | "xmpps" | "zulip") => List("to"),
    ("targets", "nctalk" | "nctalks" | "smpp" | "smpps") => At("targets"),
    ("targets", "46elks" | "amqp" | "amqps" | "aprs" | "atalk" | "azure" | "bark" | "barks" | "betterstack" | "brevo" | "bulksms" | "bulkvs" | "burstsms" | "clickatell" | "clicksend" | "cliq" | "cm" | "d7sms" | "dapnet" | "deltachat" | "deltachats" | "dingtalk" | "eight00com" | "elks" | "evolution" | "evolutions" | "exotel" | "fcm" | "flock" | "goalert" | "goalerts" | "gorush" | "gorushs" | "groundmail" | "gupshup" | "hassio" | "hassios" | "httpsms" | "humhub" | "humhubs" | "infobip" | "irc" | "ircs" | "jira" | "join" | "kakao" | "kakaotalk" | "kaleyra" | "kavenegar" | "klr" | "kook" | "line" | "mailersend" | "mailgun" | "mailto" | "mailtos" | "mastodon" | "mastodons" | "matrix" | "matrixs" | "mmost" | "mmosts" | "mobilemessage" | "mobilemsg" | "mqtt" | "mqtts" | "msg91" | "msgbird" | "ncloud" | "nclouds" | "nexmo" | "notifiarr" | "notifyre" | "ntfy" | "ntfys" | "o365" | "octopush" | "onebot" | "onebots" | "onesignal" | "opsgenie" | "pbul" | "pingram" | "plivo" | "postmark" | "pover" | "psafer" | "psafers" | "pushed" | "pushplus" | "pushy" | "reddit" | "resend" | "revolt" | "ringc" | "rocket" | "rockets" | "sendgrid" | "sendpulse" | "serwersms" | "ses" | "session" | "sessions" | "seven" | "sfr" | "signal" | "signalgrid" | "signals" | "simplepush" | "sinch" | "slack" | "smsc" | "smseagle" | "smseagles" | "smsmanager" | "smsmgr" | "smtp2go" | "sns" | "sogs" | "sparkpost" | "spush" | "telesign" | "telnyx" | "tgram" | "threema" | "toot" | "toots" | "trigv" | "trigvs" | "tweet" | "twilio" | "twist" | "twitter" | "vapid" | "viber" | "voipms" | "vonage" | "webex" | "wechat" | "wecom" | "whapi" | "whatsapp" | "wpush" | "wxpusher" | "wxteams" | "x" | "xmatters" | "xmpp" | "xmpps" | "zalo" | "zcliq" | "zohocliq" | "zulip") => List("to"),
    ("template", "msg91") => At("template"),
    ("thread_key", "gchat") => Arg("thread"),
    ("to_channel", "flock") => List("to"),
    ("to_user", "flock") => List("to"),
    ("token", "apprise" | "apprises") => Arg("to"),
    ("token", "flock" | "gotify" | "gotifys" | "lark" | "mmost" | "mmosts" | "pover" | "qq" | "ryver" | "schan" | "simplepush" | "spike" | "spugpush" | "spush") => At("token"),
    ("token", "rocket" | "rockets") => Part("password"),
    ("token", "slack") => UrlOnly,
    ("token_id", "bulksms") => Part("user"),
    ("token_secret", "bulksms") => Part("password"),
    ("topic", "pinglet" | "pinglets") => At("topic"),
    ("topic", "mqtt" | "mqtts" | "ntfy" | "ntfys") => List("to"),
    ("user_key", "pover") => At("user_key"),
    ("webhook_id", "discord" | "guilded" | "zoom") => At("webhook_id"),
    ("webhook_key", "gchat") => Arg("key"),
    ("webhook_token", "gchat") => Arg("token"),
    ("webhook_token", "discord" | "guilded") => At("webhook_token"),
    ("workflow", "workflow" | "workflows") => Arg("id"),
    _ => return None,
  })
}

/// Where the constructor argument `name` of a `schema` service sits in
/// its URL ([`UrlToken::At`]).
#[rustfmt::skip]
pub fn url_position(schema: &str, name: &str) -> Option<UrlPosition> {
  use Bound::*;
  Some(match (name, schema) {
    ("access_token", "strmlabs" | "webex" | "wxteams") => UrlPosition { place: Place::Part("host"), overrides: &["access_token"] },
    ("accesstoken", "pbul") => UrlPosition { place: Place::Part("host"), overrides: &["accesstoken"] },
    ("akey", "tweet" | "twitter" | "x") => UrlPosition { place: Place::Path { start: Start(1), end: Start(2), kind: Segments::One }, overrides: &["akey"] },
    ("apikey", "brevo" | "dot" | "evolution" | "evolutions" | "flowtriq" | "flowtriqs" | "mailersend" | "postmark" | "sendgrid") => UrlPosition { place: Place::Part("user"), overrides: &["apikey"] },
    ("apikey", "fcm" | "jira" | "kavenegar" | "msgbird" | "notifyre" | "onesignal" | "opsgenie" | "pagerduty" | "prowl" | "seven") => UrlPosition { place: Place::Part("host"), overrides: &["apikey"] },
    ("apikey", "mailgun" | "smtp2go" | "sparkpost") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["apikey"] },
    ("app_id", "reddit") => UrlPosition { place: Place::Part("host"), overrides: &["app_id"] },
    ("app_key", "pushed") => UrlPosition { place: Place::Part("host"), overrides: &["app_key"] },
    ("app_secret", "reddit") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["app_secret"] },
    ("appuser", "atalk") => UrlPosition { place: Place::Part("user"), overrides: &["appuser"] },
    ("asecret", "tweet" | "twitter" | "x") => UrlPosition { place: Place::Path { start: Start(2), end: Start(3), kind: Segments::One }, overrides: &["asecret"] },
    ("authkey", "msg91") => UrlPosition { place: Place::Part("host"), overrides: &["authkey"] },
    ("bot_token", "tgram") => UrlPosition { place: Place::Part("host"), overrides: &["bot_token"] },
    ("botname", "zulip") => UrlPosition { place: Place::Part("user"), overrides: &["botname"] },
    ("ckey", "tweet" | "twitter" | "x") => UrlPosition { place: Place::Part("host"), overrides: &["ckey"] },
    ("client_id", "kakao" | "kakaotalk" | "ringc") => UrlPosition { place: Place::Part("host"), overrides: &["client_id"] },
    ("component", "pagerduty") => UrlPosition { place: Place::Path { start: Start(1), end: Start(2), kind: Segments::One }, overrides: &["component"] },
    ("csecret", "tweet" | "twitter" | "x") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["csecret"] },
    ("device_id", "dot") => UrlPosition { place: Place::Part("host"), overrides: &["device_id"] },
    ("fullpath", "enigma2" | "enigma2s") => UrlPosition { place: Place::Path { start: Start(0), end: End(0), kind: Segments::Path }, overrides: &["fullpath"] },
    ("fullpath", "gotify" | "gotifys" | "notica" | "noticas") => UrlPosition { place: Place::Path { start: Start(0), end: End(1), kind: Segments::Path }, overrides: &["path"] },
    ("fullpath", "mmost" | "mmosts") => UrlPosition { place: Place::Path { start: Start(0), end: End(1), kind: Segments::Path }, overrides: &["fullpath"] },
    ("fullpath", "pinglet" | "pinglets") => UrlPosition { place: Place::Path { start: Start(0), end: End(2), kind: Segments::Path }, overrides: &["path"] },
    ("instance", "evolution" | "evolutions") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["instance"] },
    ("integration_id", "xmatters") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["integration_id"] },
    ("integrationkey", "pagerduty") => UrlPosition { place: Place::Part("user"), overrides: &["integrationkey"] },
    ("kind", "github") => UrlPosition { place: Place::Path { start: Start(1), end: Start(2), kind: Segments::One }, overrides: &["kind"] },
    ("namespace", "pinglet" | "pinglets") => UrlPosition { place: Place::Path { start: End(2), end: End(1), kind: Segments::One }, overrides: &["namespace"] },
    ("number", "github") => UrlPosition { place: Place::Path { start: Start(2), end: Start(3), kind: Segments::One }, overrides: &["number"] },
    ("organization", "ryver" | "zulip") => UrlPosition { place: Place::Part("host"), overrides: &["organization"] },
    ("owner", "github") => UrlPosition { place: Place::Part("host"), overrides: &["owner"] },
    ("privatekey", "psafer" | "psafers") => UrlPosition { place: Place::Part("host"), overrides: &["privatekey"] },
    ("project", "fcm") => UrlPosition { place: Place::Part("host"), overrides: &["project"] },
    ("providerkey", "prowl") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["providerkey"] },
    ("refresh_token", "kakao" | "kakaotalk") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["refresh_token"] },
    ("repo", "github") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["repo"] },
    ("secret", "signl4") => UrlPosition { place: Place::Part("host"), overrides: &["secret"] },
    ("serial", "blink1") => UrlPosition { place: Place::Part("host"), overrides: &["serial"] },
    ("serverkey", "kumulos") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["serverkey"] },
    ("source", "pagerduty") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["from", "source"] },
    ("source", "smpp" | "smpps") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["from", "from_phone", "source"] },
    ("space_id", "sfr") => UrlPosition { place: Place::Part("host"), overrides: &["space_id"] },
    ("targets", "nctalk" | "nctalks") => UrlPosition { place: Place::Path { start: Start(0), end: End(0), kind: Segments::List }, overrides: &["target_room_id", "targets", "to"] },
    ("targets", "smpp" | "smpps") => UrlPosition { place: Place::Path { start: Start(1), end: End(0), kind: Segments::List }, overrides: &["target_phone", "targets", "to"] },
    ("template", "msg91") => UrlPosition { place: Place::Part("user"), overrides: &["template"] },
    ("token", "flock" | "lark" | "pover" | "qq" | "schan" | "simplepush" | "spike" | "spugpush" | "spush") => UrlPosition { place: Place::Part("host"), overrides: &["token"] },
    ("token", "gotify" | "gotifys" | "mmost" | "mmosts") => UrlPosition { place: Place::Path { start: End(1), end: End(0), kind: Segments::One }, overrides: &["token"] },
    ("token", "ryver") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["token"] },
    ("topic", "pinglet" | "pinglets") => UrlPosition { place: Place::Path { start: End(1), end: End(0), kind: Segments::One }, overrides: &["topic"] },
    ("user_key", "pover") => UrlPosition { place: Place::Part("user"), overrides: &["user_key"] },
    ("webhook_id", "discord" | "guilded" | "zoom") => UrlPosition { place: Place::Part("host"), overrides: &["webhook_id"] },
    ("webhook_path", "flowtriq" | "flowtriqs") => UrlPosition { place: Place::Path { start: Start(0), end: End(0), kind: Segments::Path }, overrides: &["path"] },
    ("webhook_token", "discord" | "guilded") => UrlPosition { place: Place::Path { start: Start(0), end: Start(1), kind: Segments::One }, overrides: &["webhook_token"] },
    _ => return None,
  })
}
