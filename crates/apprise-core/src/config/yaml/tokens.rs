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
  /// Several URL values: only the URL can set them.
  UrlOnly,
}

/// What the YAML setting `key` of a `schema` service (lowercase) sets,
/// or `None` for a setting applied as the URL argument of its name.
#[rustfmt::skip]
pub fn url_token(schema: &str, key: &str) -> Option<UrlToken> {
  use UrlToken::*;
  Some(match (key, schema) {
    ("access_key_id", "ses" | "sns") => Arg("access"),
    ("accesstoken", "hassio" | "hassios") => Arg("token"),
    ("account_sid", "twilio") => Arg("sid"),
    ("api_key", "octopush") => Arg("key"),
    ("api_login", "octopush") => Arg("login"),
    ("api_token", "sinch") => Arg("token"),
    ("apikey", "burstsms" | "gupshup" | "httpsms" | "infobip" | "kaleyra" | "klr" | "nexmo" | "pushy" | "smsmanager" | "smsmgr" | "telnyx" | "vonage") => Arg("key"),
    ("apikey", "clicksend" | "telesign") => Part("password"),
    ("auth_id", "plivo") => Arg("id"),
    ("auth_token", "twilio") => Arg("token"),
    ("botname", "discord" | "flock" | "fluxer" | "fluxers" | "guilded" | "ryver" | "slack") => Part("user"),
    ("client_id", "sendpulse") => Arg("id"),
    ("client_id", "azure" | "o365") => Arg("oauth_id"),
    ("client_secret", "sendpulse") => Arg("secret"),
    ("customer_id", "telesign") => Part("user"),
    ("device", "join") => List("to"),
    ("device_name", "join") => List("to"),
    ("events", "ifttt") => List("to"),
    ("from_addr", "betterstack" | "groundmail" | "resend") => Arg("from"),
    ("from_email", "ses") => Arg("from"),
    ("from_phone", "46elks" | "bulkvs" | "eight00com" | "elks" | "exotel" | "httpsms" | "nexmo" | "ringc" | "signal" | "signals" | "sinch" | "telnyx" | "twilio" | "voipms" | "vonage") => Arg("from"),
    ("from_phone_id", "whatsapp") => Arg("from"),
    ("gateway_id", "threema") => Part("user"),
    ("group", "join") => List("to"),
    ("language", "whatsapp") => Arg("lang"),
    ("msghook", "notifico" | "notificos") => Arg("token"),
    ("project_id", "notifico" | "notificos") => Arg("project"),
    ("secret", "azure" | "o365") => Arg("oauth_secret"),
    ("secret_access_key", "ses" | "sns") => Arg("secret"),
    ("secret_key", "pjet" | "pjets") => Arg("secret"),
    ("sender", "mobilemessage" | "mobilemsg") => Arg("from"),
    ("sender_id", "burstsms") => Arg("from"),
    ("service_plan_id", "sinch") => Arg("spi"),
    ("short_code", "sinch" | "twilio") => List("to"),
    ("signature", "workflow" | "workflows") => Arg("sig"),
    ("source", "clickatell" | "cm" | "kavenegar" | "msgbird" | "plivo") => Arg("from"),
    ("subscriber", "vapid") => Arg("from"),
    ("target", "sfr") => List("to"),
    ("target_bot", "cliq" | "zcliq" | "zohocliq") => List("to"),
    ("target_callsign", "aprs" | "dapnet") => List("to"),
    ("target_channel", "cliq" | "irc" | "ircs" | "mmost" | "mmosts" | "notifiarr" | "pbul" | "pushed" | "revolt" | "rocket" | "rockets" | "signal" | "signals" | "trigv" | "trigvs" | "twist" | "zcliq" | "zohocliq") => List("to"),
    ("target_channel_id", "mmost" | "mmosts" | "twist") => List("to"),
    ("target_channels", "slack" | "xmpp" | "xmpps") => List("to"),
    ("target_chat_id", "zalo") => List("to"),
    ("target_contact", "smseagle" | "smseagles") => List("to"),
    ("target_department", "wechat") => List("to"),
    ("target_device", "bark" | "barks" | "fcm" | "gorush" | "gorushs" | "hassio" | "hassios" | "pbul" | "pover" | "psafer" | "psafers" | "pushy") => List("to"),
    ("target_email", "azure" | "brevo" | "deltachat" | "deltachats" | "groundmail" | "mailersend" | "mailgun" | "mailto" | "mailtos" | "o365" | "onesignal" | "pbul" | "pingram" | "postmark" | "psafer" | "psafers" | "resend" | "sendgrid" | "sendpulse" | "slack" | "threema") => List("to"),
    ("target_encoded_id", "slack") => List("to"),
    ("target_escalation", "jira" | "opsgenie") => List("to"),
    ("target_group", "bulksms" | "ncloud" | "nclouds" | "onebot" | "onebots" | "pover" | "serwersms" | "smseagle" | "smseagles" | "whatsapp") => List("to"),
    ("target_id", "pingram") => List("to"),
    ("target_member", "simplepush" | "spush") => List("to"),
    ("target_phone", "46elks" | "atalk" | "bulksms" | "bulkvs" | "burstsms" | "clickatell" | "clicksend" | "cm" | "d7sms" | "eight00com" | "elks" | "evolution" | "evolutions" | "exotel" | "gupshup" | "httpsms" | "infobip" | "kaleyra" | "kavenegar" | "klr" | "mobilemessage" | "mobilemsg" | "msg91" | "msgbird" | "nexmo" | "notifyre" | "octopush" | "plivo" | "ringc" | "serwersms" | "seven" | "signal" | "signals" | "sinch" | "smsc" | "smseagle" | "smseagles" | "smsmanager" | "smsmgr" | "telesign" | "telnyx" | "threema" | "twilio" | "voipms" | "vonage" | "whatsapp") => List("to"),
    ("target_phone_no", "dingtalk" | "sns") => List("to"),
    ("target_player", "onesignal") => List("to"),
    ("target_room", "rocket" | "rockets") => List("to"),
    ("target_room_alias", "matrix" | "matrixs") => List("to"),
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
    ("targets", "46elks" | "amqp" | "amqps" | "aprs" | "atalk" | "azure" | "bark" | "barks" | "betterstack" | "brevo" | "bulksms" | "bulkvs" | "burstsms" | "clickatell" | "clicksend" | "cliq" | "cm" | "d7sms" | "dapnet" | "deltachat" | "deltachats" | "dingtalk" | "eight00com" | "elks" | "evolution" | "evolutions" | "exotel" | "fcm" | "flock" | "goalert" | "goalerts" | "gorush" | "gorushs" | "groundmail" | "gupshup" | "hassio" | "hassios" | "httpsms" | "humhub" | "humhubs" | "infobip" | "irc" | "ircs" | "jira" | "join" | "kaleyra" | "kavenegar" | "klr" | "kook" | "line" | "mailersend" | "mailgun" | "mailto" | "mailtos" | "mastodon" | "mastodons" | "matrix" | "matrixs" | "mmost" | "mmosts" | "mobilemessage" | "mobilemsg" | "mqtt" | "mqtts" | "msg91" | "msgbird" | "ncloud" | "nclouds" | "nexmo" | "notifiarr" | "notifyre" | "ntfy" | "ntfys" | "o365" | "octopush" | "onebot" | "onebots" | "onesignal" | "opsgenie" | "pbul" | "pingram" | "plivo" | "postmark" | "pover" | "psafer" | "psafers" | "pushed" | "pushplus" | "pushy" | "reddit" | "resend" | "revolt" | "ringc" | "rocket" | "rockets" | "sendgrid" | "sendpulse" | "serwersms" | "ses" | "session" | "sessions" | "seven" | "sfr" | "signal" | "signalgrid" | "signals" | "simplepush" | "sinch" | "slack" | "smsc" | "smseagle" | "smseagles" | "smsmanager" | "smsmgr" | "smtp2go" | "sns" | "sogs" | "sparkpost" | "spush" | "telesign" | "telnyx" | "tgram" | "threema" | "toot" | "toots" | "trigv" | "trigvs" | "tweet" | "twilio" | "twist" | "twitter" | "vapid" | "viber" | "voipms" | "vonage" | "webex" | "wechat" | "wecom" | "whatsapp" | "wpush" | "wxpusher" | "wxteams" | "x" | "xmpp" | "xmpps" | "zalo" | "zcliq" | "zohocliq" | "zulip") => List("to"),
    ("thread_key", "gchat") => Arg("thread"),
    ("to_channel", "flock") => List("to"),
    ("to_user", "flock") => List("to"),
    ("token", "apprise" | "apprises") => Arg("to"),
    ("token", "rocket" | "rockets") => Part("password"),
    ("token", "slack") => UrlOnly,
    ("token_id", "bulksms") => Part("user"),
    ("token_secret", "bulksms") => Part("password"),
    ("topic", "mqtt" | "mqtts" | "ntfy" | "ntfys") => List("to"),
    ("webhook_key", "gchat") => Arg("key"),
    ("webhook_token", "gchat") => Arg("token"),
    ("workflow", "workflow" | "workflows") => Arg("id"),
    _ => return None,
  })
}
