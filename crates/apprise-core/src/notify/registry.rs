use std::collections::HashMap;
use std::sync::OnceLock;

use super::{ConfiguredService, Notify, ServiceOptions};
use crate::utils::cwe312::cwe312_url;
use crate::utils::parse::{ParsedUrl, escape_path_hash};

pub type FactoryFn = fn(&ParsedUrl) -> Option<Box<dyn Notify>>;

/// Global schema → factory mapping
pub fn registry() -> &'static HashMap<String, FactoryFn> {
  static REGISTRY: OnceLock<HashMap<String, FactoryFn>> = OnceLock::new();
  REGISTRY.get_or_init(build_registry)
}

/// All service details, ordered alphabetically by service name
pub fn all_service_details() -> Vec<super::ServiceDetails> {
  let mut seen = std::collections::HashSet::new();
  let mut details = Vec::new();

  // Collect from each plugin's static details() function
  macro_rules! collect {
    ($plugin:expr) => {
      if seen.insert($plugin.service_name) {
        details.push($plugin);
      }
    };
  }

  use super::*;
  collect!(africas_talking::AfricasTalking::static_details());
  #[cfg(feature = "amqp")]
  collect!(amqp::Amqp::static_details());
  collect!(apprise_api::AppriseApi::static_details());
  collect!(aprs::Aprs::static_details());
  collect!(bark::Bark::static_details());
  collect!(betterstack::BetterStack::static_details());
  collect!(blink1::Blink1::static_details());
  collect!(bluesky::BlueSky::static_details());
  collect!(brevo::Brevo::static_details());
  collect!(bulksms::BulkSms::static_details());
  collect!(bulkvs::BulkVs::static_details());
  collect!(burstsms::BurstSms::static_details());
  collect!(chanify::Chanify::static_details());
  collect!(clickatell::Clickatell::static_details());
  collect!(clicksend::ClickSend::static_details());
  collect!(cm::Cm::static_details());
  collect!(d7networks::D7Networks::static_details());
  collect!(dapnet::Dapnet::static_details());
  #[cfg(feature = "email")]
  collect!(deltachat::DeltaChat::static_details());
  #[cfg(all(feature = "desktop", unix))]
  collect!(desktop::dbus::DBus::static_details());
  #[cfg(all(feature = "desktop", unix))]
  collect!(desktop::glib::GLib::static_details());
  #[cfg(all(feature = "desktop", unix))]
  collect!(desktop::gnome::Gnome::static_details());
  #[cfg(all(feature = "desktop", target_os = "macos"))]
  collect!(desktop::macosx::MacOSX::static_details());
  #[cfg(all(feature = "desktop", windows))]
  collect!(desktop::windows::Windows::static_details());
  collect!(dingtalk::DingTalk::static_details());
  collect!(discord::Discord::static_details());
  collect!(dot::Dot::static_details());
  collect!(eight00com::Eight00com::static_details());
  collect!(emby::Emby::static_details());
  collect!(enigma2::Enigma2::static_details());
  collect!(exotel::Exotel::static_details());
  collect!(fcm::Fcm::static_details());
  collect!(feishu::FeiShu::static_details());
  collect!(flock::Flock::static_details());
  collect!(flowtriq::Flowtriq::static_details());
  collect!(fluxer::Fluxer::static_details());
  collect!(fortysixelks::FortySixElks::static_details());
  collect!(freemobile::FreeMobile::static_details());
  collect!(github::GitHub::static_details());
  collect!(goalert::GoAlert::static_details());
  collect!(google_chat::GoogleChat::static_details());
  collect!(gorush::Gorush::static_details());
  collect!(gotify::Gotify::static_details());
  collect!(groundmail::Groundmail::static_details());
  collect!(groupme::GroupMe::static_details());
  collect!(growl::Growl::static_details());
  collect!(guilded::Guilded::static_details());
  collect!(gupshup::Gupshup::static_details());
  collect!(home_assistant::HomeAssistant::static_details());
  collect!(httpsms::HttpSms::static_details());
  collect!(humhub::HumHub::static_details());
  collect!(ifttt::Ifttt::static_details());
  collect!(ilert::ILert::static_details());
  collect!(incidentio::IncidentIO::static_details());
  collect!(infobip::Infobip::static_details());
  collect!(irc::Irc::static_details());
  collect!(jellyfin::Jellyfin::static_details());
  collect!(jira::Jira::static_details());
  collect!(join::Join::static_details());
  collect!(custom_json::Json::static_details());
  collect!(custom_form::Form::static_details());
  collect!(custom_xml::Xml::static_details());
  collect!(kaleyra::Kaleyra::static_details());
  collect!(kavenegar::Kavenegar::static_details());
  collect!(kodi::Kodi::static_details());
  collect!(kumulos::Kumulos::static_details());
  collect!(lametric::LaMetric::static_details());
  collect!(lark::Lark::static_details());
  collect!(lauther::Lauther::static_details());
  collect!(lfgalarm::LfgAlarm::static_details());
  collect!(line::Line::static_details());
  collect!(mailersend::MailerSend::static_details());
  collect!(mailgun::Mailgun::static_details());
  collect!(mastodon::Mastodon::static_details());
  collect!(matrix::Matrix::static_details());
  collect!(mattermost::Mattermost::static_details());
  collect!(messagebird::MessageBird::static_details());
  collect!(misskey::Misskey::static_details());
  collect!(mobilemessage::MobileMessage::static_details());
  #[cfg(feature = "mqtt")]
  collect!(mqtt::Mqtt::static_details());
  collect!(msg91::Msg91::static_details());
  collect!(nextcloud::Nextcloud::static_details());
  collect!(nextcloudtalk::NextcloudTalk::static_details());
  collect!(notica::Notica::static_details());
  collect!(notifiarr::Notifiarr::static_details());
  collect!(notifico::Notifico::static_details());
  collect!(notifyre::Notifyre::static_details());
  collect!(ntfy::Ntfy::static_details());
  collect!(octopush::Octopush::static_details());
  collect!(office365::Office365::static_details());
  collect!(one_signal::OneSignal::static_details());
  collect!(onebot::OneBot::static_details());
  collect!(opsgenie::Opsgenie::static_details());
  collect!(pagerduty::PagerDuty::static_details());
  collect!(pagertree::PagerTree::static_details());
  collect!(parseplatform::ParsePlatform::static_details());
  collect!(pinglet::Pinglet::static_details());
  collect!(pingram::Pingram::static_details());
  collect!(plivo::Plivo::static_details());
  collect!(postmark::Postmark::static_details());
  collect!(prowl::Prowl::static_details());
  collect!(pushbullet::Pushbullet::static_details());
  collect!(pushdeer::PushDeer::static_details());
  collect!(pushed::Pushed::static_details());
  collect!(pushjet::PushJet::static_details());
  collect!(pushme::PushMe::static_details());
  collect!(pushover::Pushover::static_details());
  collect!(pushplus::PushPlus::static_details());
  collect!(pushsafer::Pushsafer::static_details());
  collect!(pushward::PushWard::static_details());
  collect!(pushy::Pushy::static_details());
  collect!(qq::Qq::static_details());
  collect!(reddit::Reddit::static_details());
  collect!(resend::Resend::static_details());
  collect!(revolt::Revolt::static_details());
  collect!(ringcentral::RingCentral::static_details());
  collect!(rocketchat::RocketChat::static_details());
  collect!(rsyslog::RSyslog::static_details());
  collect!(ryver::Ryver::static_details());
  collect!(sendgrid::SendGrid::static_details());
  collect!(sendpulse::SendPulse::static_details());
  collect!(serverchan::ServerChan::static_details());
  collect!(serwersms::SerwerSms::static_details());
  collect!(ses::Ses::static_details());
  collect!(seven::Seven::static_details());
  collect!(sfr::Sfr::static_details());
  collect!(signal_api::SignalApi::static_details());
  collect!(signalgrid::Signalgrid::static_details());
  collect!(signl4::Signl4::static_details());
  collect!(simplepush::SimplePush::static_details());
  collect!(sinch::Sinch::static_details());
  collect!(slack::Slack::static_details());
  collect!(smpp::Smpp::static_details());
  collect!(smsc::Smsc::static_details());
  collect!(smseagle::SmsEagle::static_details());
  collect!(smsmanager::SmsManager::static_details());
  collect!(smtp2go::Smtp2Go::static_details());
  collect!(sns::Sns::static_details());
  collect!(sogs::SessionOgs::static_details());
  collect!(sparkpost::SparkPost::static_details());
  collect!(spike::Spike::static_details());
  collect!(splunk::Splunk::static_details());
  collect!(spugpush::SpugPush::static_details());
  collect!(squadcast::Squadcast::static_details());
  collect!(stackfield::Stackfield::static_details());
  collect!(streamlabs::Streamlabs::static_details());
  collect!(synology::Synology::static_details());
  collect!(syslog::Syslog::static_details());
  collect!(techuluspush::TechulusPush::static_details());
  collect!(telegram::Telegram::static_details());
  collect!(telesign::Telesign::static_details());
  collect!(telnyx::Telnyx::static_details());
  collect!(threema::Threema::static_details());
  collect!(trigv::Trigv::static_details());
  collect!(twilio::Twilio::static_details());
  collect!(twist::Twist::static_details());
  collect!(twitter::Twitter::static_details());
  collect!(viber::Viber::static_details());
  collect!(voipms::VoipMs::static_details());
  collect!(vapid::Vapid::static_details());
  collect!(vonage::Vonage::static_details());
  collect!(webexteams::WebexTeams::static_details());
  collect!(wechat::WeChat::static_details());
  collect!(wecombot::WeComBot::static_details());
  collect!(whatsapp::WhatsApp::static_details());
  collect!(workflows::Workflows::static_details());
  collect!(wpush::WPush::static_details());
  collect!(wxpusher::WxPusher::static_details());
  collect!(xmpp::Xmpp::static_details());
  collect!(zalo::Zalo::static_details());
  collect!(youlmk::YouLmk::static_details());
  collect!(zoom::Zoom::static_details());
  collect!(zulip::Zulip::static_details());
  #[cfg(feature = "email")]
  collect!(email::Email::static_details());

  details.sort_by(|a, b| a.service_name.cmp(b.service_name));
  details
}

fn build_registry() -> HashMap<String, FactoryFn> {
  let mut m: HashMap<String, FactoryFn> = HashMap::new();

  macro_rules! reg {
        ($module:path, $($schema:expr),+ $(,)?) => {
            $(m.insert($schema.to_string(), |u| $module(u).map(|p| Box::new(p) as Box<dyn Notify>));)+
        };
    }

  use super::*;

  reg!(africas_talking::AfricasTalking::from_url, "atalk");
  #[cfg(feature = "amqp")]
  reg!(amqp::Amqp::from_url, "amqp", "amqps");
  reg!(apprise_api::AppriseApi::from_url, "apprise", "apprises");
  reg!(aprs::Aprs::from_url, "aprs");
  reg!(bark::Bark::from_url, "bark", "barks");
  reg!(betterstack::BetterStack::from_url, "betterstack");
  reg!(blink1::Blink1::from_url, "blink1");
  reg!(bluesky::BlueSky::from_url, "bsky", "bluesky");
  reg!(brevo::Brevo::from_url, "brevo");
  reg!(bulksms::BulkSms::from_url, "bulksms");
  reg!(bulkvs::BulkVs::from_url, "bulkvs");
  reg!(burstsms::BurstSms::from_url, "burstsms");
  reg!(chanify::Chanify::from_url, "chanify");
  reg!(clickatell::Clickatell::from_url, "clickatell");
  reg!(clicksend::ClickSend::from_url, "clicksend");
  reg!(cm::Cm::from_url, "cm");
  reg!(d7networks::D7Networks::from_url, "d7sms");
  reg!(dapnet::Dapnet::from_url, "dapnet");
  #[cfg(feature = "email")]
  reg!(deltachat::DeltaChat::from_url, "deltachat", "deltachats");
  #[cfg(all(feature = "desktop", unix))]
  reg!(desktop::dbus::DBus::from_url, "dbus", "kde", "qt");
  #[cfg(all(feature = "desktop", unix))]
  reg!(desktop::glib::GLib::from_url, "glib", "gio");
  #[cfg(all(feature = "desktop", unix))]
  reg!(desktop::gnome::Gnome::from_url, "gnome");
  #[cfg(all(feature = "desktop", target_os = "macos"))]
  reg!(desktop::macosx::MacOSX::from_url, "macosx");
  #[cfg(all(feature = "desktop", windows))]
  reg!(desktop::windows::Windows::from_url, "windows");
  reg!(dingtalk::DingTalk::from_url, "dingtalk");
  reg!(discord::Discord::from_url, "discord");
  reg!(dot::Dot::from_url, "dot");
  reg!(eight00com::Eight00com::from_url, "eight00com");
  reg!(emby::Emby::from_url, "emby", "embys");
  reg!(enigma2::Enigma2::from_url, "enigma2", "enigma2s");
  reg!(exotel::Exotel::from_url, "exotel");
  reg!(fcm::Fcm::from_url, "fcm");
  reg!(feishu::FeiShu::from_url, "feishu");
  reg!(flock::Flock::from_url, "flock");
  reg!(flowtriq::Flowtriq::from_url, "flowtriq", "flowtriqs");
  reg!(fluxer::Fluxer::from_url, "fluxer", "fluxers");
  reg!(fortysixelks::FortySixElks::from_url, "46elks", "elks");
  reg!(freemobile::FreeMobile::from_url, "freemobile");
  reg!(github::GitHub::from_url, "github");
  reg!(goalert::GoAlert::from_url, "goalert", "goalerts");
  reg!(google_chat::GoogleChat::from_url, "gchat");
  reg!(gorush::Gorush::from_url, "gorush", "gorushs");
  reg!(gotify::Gotify::from_url, "gotify", "gotifys");
  reg!(groundmail::Groundmail::from_url, "groundmail");
  reg!(groupme::GroupMe::from_url, "groupme");
  reg!(growl::Growl::from_url, "growl");
  reg!(guilded::Guilded::from_url, "guilded");
  reg!(gupshup::Gupshup::from_url, "gupshup");
  reg!(home_assistant::HomeAssistant::from_url, "hassio", "hassios");
  reg!(httpsms::HttpSms::from_url, "httpsms");
  reg!(humhub::HumHub::from_url, "humhub", "humhubs");
  reg!(ifttt::Ifttt::from_url, "ifttt");
  reg!(ilert::ILert::from_url, "ilert");
  reg!(incidentio::IncidentIO::from_url, "incidentio");
  reg!(infobip::Infobip::from_url, "infobip");
  reg!(irc::Irc::from_url, "irc", "ircs");
  reg!(jellyfin::Jellyfin::from_url, "jellyfin", "jellyfins");
  reg!(jira::Jira::from_url, "jira");
  reg!(join::Join::from_url, "join");
  reg!(custom_json::Json::from_url, "json", "jsons");
  reg!(custom_form::Form::from_url, "form", "forms");
  reg!(custom_xml::Xml::from_url, "xml", "xmls");
  reg!(kaleyra::Kaleyra::from_url, "kaleyra", "klr");
  reg!(kavenegar::Kavenegar::from_url, "kavenegar");
  reg!(kodi::Kodi::from_url, "kodi", "kodis", "xbmc", "xbmcs");
  reg!(kumulos::Kumulos::from_url, "kumulos");
  reg!(lametric::LaMetric::from_url, "lametric", "lametrics");
  reg!(lark::Lark::from_url, "lark");
  reg!(lauther::Lauther::from_url, "lauther");
  reg!(lfgalarm::LfgAlarm::from_url, "lfg");
  reg!(line::Line::from_url, "line");
  reg!(mailersend::MailerSend::from_url, "mailersend");
  reg!(mailgun::Mailgun::from_url, "mailgun");
  reg!(mastodon::Mastodon::from_url, "mastodon", "toot", "mastodons", "toots");
  reg!(matrix::Matrix::from_url, "matrix", "matrixs");
  reg!(mattermost::Mattermost::from_url, "mmost", "mmosts");
  reg!(messagebird::MessageBird::from_url, "msgbird");
  reg!(misskey::Misskey::from_url, "misskey", "misskeys");
  reg!(mobilemessage::MobileMessage::from_url, "mobilemessage", "mobilemsg");
  #[cfg(feature = "mqtt")]
  reg!(mqtt::Mqtt::from_url, "mqtt", "mqtts");
  reg!(msg91::Msg91::from_url, "msg91");
  reg!(nextcloud::Nextcloud::from_url, "ncloud", "nclouds");
  reg!(nextcloudtalk::NextcloudTalk::from_url, "nctalk", "nctalks");
  reg!(notica::Notica::from_url, "notica", "noticas");
  reg!(notifiarr::Notifiarr::from_url, "notifiarr");
  reg!(notifyre::Notifyre::from_url, "notifyre");
  reg!(notifico::Notifico::from_url, "notifico", "notificos");
  reg!(ntfy::Ntfy::from_url, "ntfy", "ntfys");
  reg!(octopush::Octopush::from_url, "octopush");
  reg!(office365::Office365::from_url, "o365", "azure");
  reg!(one_signal::OneSignal::from_url, "onesignal");
  reg!(onebot::OneBot::from_url, "onebot", "onebots");
  reg!(opsgenie::Opsgenie::from_url, "opsgenie");
  reg!(pagerduty::PagerDuty::from_url, "pagerduty");
  reg!(pagertree::PagerTree::from_url, "pagertree");
  reg!(parseplatform::ParsePlatform::from_url, "parsep", "parseps");
  reg!(pinglet::Pinglet::from_url, "pinglet", "pinglets");
  reg!(pingram::Pingram::from_url, "pingram");
  reg!(plivo::Plivo::from_url, "plivo");
  reg!(postmark::Postmark::from_url, "postmark");
  reg!(prowl::Prowl::from_url, "prowl");
  reg!(pushbullet::Pushbullet::from_url, "pbul");
  reg!(pushdeer::PushDeer::from_url, "pushdeer", "pushdeers");
  reg!(pushed::Pushed::from_url, "pushed");
  reg!(pushjet::PushJet::from_url, "pjet", "pjets");
  reg!(pushme::PushMe::from_url, "pushme");
  reg!(pushover::Pushover::from_url, "pover");
  reg!(pushplus::PushPlus::from_url, "pushplus", "wecom");
  reg!(pushsafer::Pushsafer::from_url, "psafer", "psafers");
  reg!(pushward::PushWard::from_url, "pushward");
  reg!(pushy::Pushy::from_url, "pushy");
  reg!(qq::Qq::from_url, "qq");
  reg!(reddit::Reddit::from_url, "reddit");
  reg!(resend::Resend::from_url, "resend");
  reg!(revolt::Revolt::from_url, "revolt");
  reg!(ringcentral::RingCentral::from_url, "ringc");
  reg!(rocketchat::RocketChat::from_url, "rocket", "rockets");
  reg!(rsyslog::RSyslog::from_url, "rsyslog");
  reg!(ryver::Ryver::from_url, "ryver");
  reg!(sendgrid::SendGrid::from_url, "sendgrid");
  reg!(sendpulse::SendPulse::from_url, "sendpulse");
  reg!(serverchan::ServerChan::from_url, "schan");
  reg!(serwersms::SerwerSms::from_url, "serwersms");
  reg!(ses::Ses::from_url, "ses");
  reg!(seven::Seven::from_url, "seven");
  reg!(sfr::Sfr::from_url, "sfr");
  reg!(signal_api::SignalApi::from_url, "signal", "signals");
  reg!(signalgrid::Signalgrid::from_url, "signalgrid");
  reg!(signl4::Signl4::from_url, "signl4");
  reg!(simplepush::SimplePush::from_url, "spush", "simplepush");
  reg!(sinch::Sinch::from_url, "sinch");
  reg!(slack::Slack::from_url, "slack");
  reg!(smpp::Smpp::from_url, "smpp", "smpps");
  reg!(smsc::Smsc::from_url, "smsc");
  reg!(smseagle::SmsEagle::from_url, "smseagle", "smseagles");
  reg!(smsmanager::SmsManager::from_url, "smsmanager", "smsmgr");
  reg!(smtp2go::Smtp2Go::from_url, "smtp2go");
  reg!(sns::Sns::from_url, "sns");
  reg!(sogs::SessionOgs::from_url, "session", "sessions", "sogs");
  reg!(sparkpost::SparkPost::from_url, "sparkpost");
  reg!(spike::Spike::from_url, "spike");
  reg!(splunk::Splunk::from_url, "splunk", "victorops");
  reg!(spugpush::SpugPush::from_url, "spugpush");
  reg!(squadcast::Squadcast::from_url, "squadcast");
  reg!(stackfield::Stackfield::from_url, "stackfield");
  reg!(streamlabs::Streamlabs::from_url, "strmlabs");
  reg!(synology::Synology::from_url, "synology", "synologys");
  reg!(syslog::Syslog::from_url, "syslog");
  reg!(techuluspush::TechulusPush::from_url, "push", "techulus");
  reg!(telegram::Telegram::from_url, "tgram");
  reg!(telesign::Telesign::from_url, "telesign");
  reg!(telnyx::Telnyx::from_url, "telnyx");
  reg!(threema::Threema::from_url, "threema");
  reg!(trigv::Trigv::from_url, "trigv", "trigvs");
  reg!(twilio::Twilio::from_url, "twilio");
  reg!(twist::Twist::from_url, "twist");
  reg!(twitter::Twitter::from_url, "twitter", "x", "tweet");
  reg!(viber::Viber::from_url, "viber");
  reg!(voipms::VoipMs::from_url, "voipms");
  reg!(vapid::Vapid::from_url, "vapid");
  reg!(vonage::Vonage::from_url, "vonage", "nexmo");
  reg!(webexteams::WebexTeams::from_url, "wxteams", "webex");
  reg!(wechat::WeChat::from_url, "wechat");
  reg!(wecombot::WeComBot::from_url, "wecombot");
  reg!(whatsapp::WhatsApp::from_url, "whatsapp");
  reg!(workflows::Workflows::from_url, "workflow", "workflows");
  reg!(wpush::WPush::from_url, "wpush");
  reg!(wxpusher::WxPusher::from_url, "wxpusher");
  reg!(xmpp::Xmpp::from_url, "xmpp", "xmpps");
  reg!(zalo::Zalo::from_url, "zalo");
  reg!(youlmk::YouLmk::from_url, "youlmk");
  reg!(zoom::Zoom::from_url, "zoom");
  reg!(zulip::Zulip::from_url, "zulip");
  #[cfg(feature = "email")]
  reg!(email::Email::from_url, "mailto", "mailtos");

  m
}

/// Instantiate a notifier from a URL string.
///
/// The URL is resolved like upstream `url_to_dict()` (see
/// [`parse_service_url`]) and the plugin is wrapped in a
/// [`ConfiguredService`] carrying the common URL options (`?retry=`,
/// `?format=`, ...). A URL whose common options are invalid does not load.
pub fn from_url(url: &str) -> Option<Box<dyn Notify>> {
  from_parsed(&parse_service_url(url)?)
}

/// Build a notifier from a URL already resolved by [`parse_service_url`]
/// (and possibly adjusted by a configuration loader).
pub fn from_parsed(parsed: &ParsedUrl) -> Option<Box<dyn Notify>> {
  let options = match ServiceOptions::from_url(parsed) {
    Ok(options) => options,
    Err(e) => {
      tracing::warn!("{}", e);
      return None;
    }
  };
  let Some(plugin) = instantiate(parsed) else {
    tracing::error!("Could not load {} URL: {}", service_name(&parsed.schema), cwe312_url(&parsed.raw));
    return None;
  };
  Some(Box::new(ConfiguredService::new(plugin, options).with_url(&parsed.raw)))
}

/// Whether a schema belongs to a registered plugin.
pub fn is_supported_schema(schema: &str) -> bool {
  registry().contains_key(&schema.to_lowercase())
}

/// The friendly service name for a schema, or the schema itself.
pub fn service_name(schema: &str) -> String {
  static NAMES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
  let names = NAMES.get_or_init(|| all_service_details().into_iter().flat_map(|d| d.protocols.into_iter().map(move |p| (p, d.service_name))).collect());
  names.get(schema).map(|s| s.to_string()).unwrap_or_else(|| format!("{}://", schema))
}

/// Upstream `GET_SCHEMA_RE` (`\s*(?P<schema>[a-z0-9]{1,32})://.*$`):
/// the lowercase schema of something that looks like a URL.
pub fn url_schema(url: &str) -> Option<String> {
  let t = url.trim_start();
  let len = t.bytes().take_while(|b| b.is_ascii_alphanumeric()).count();
  if !(1..=32).contains(&len) || !t[len..].starts_with("://") {
    return None;
  }
  // `.` does not cross a newline; `$` may sit before a final one.
  let rest = &t[len + 3..];
  let rest = rest.strip_suffix('\n').unwrap_or(rest);
  if rest.contains('\n') {
    return None;
  }
  Some(t[..len].to_lowercase())
}

/// Resolve a URL to the plugin URL that describes it (upstream
/// `url_to_dict()`).
///
/// A URL whose schema belongs to a plugin is parsed directly. Any other
/// URL (typically the `https://` webhook a service hands out) is matched
/// against [`NATIVE_URLS`] and converted to the plugin's own form. Returns
/// `None`, after logging why, when neither works.
pub fn parse_service_url(url: &str) -> Option<ParsedUrl> {
  let url_ = escape_path_hash(url);
  let Some(schema) = url_schema(&url_) else {
    tracing::error!("Unsupported URL: {}", cwe312_url(url));
    return None;
  };

  if registry().contains_key(&schema) {
    let parsed = ParsedUrl::parse(&url_);
    if parsed.is_none() {
      tracing::error!("Unparseable {} URL {}", service_name(&schema), cwe312_url(url));
    }
    return parsed;
  }

  let reg = registry();
  for native in NATIVE_URLS {
    if !reg.contains_key(native.schema) {
      continue;
    }
    if let Some(converted) = (native.convert)(&url_)
      && let Some(parsed) = ParsedUrl::parse(&converted)
    {
      tracing::trace!("URL {} unpacked as {}", cwe312_url(url), cwe312_url(&converted));
      return Some(parsed);
    }
  }

  tracing::error!("Unparseable URL {}", cwe312_url(url));
  None
}

/// Find the plugin for a parsed URL and build it, without the common
/// options. Native `https://` webhook URLs are converted earlier, by
/// [`parse_service_url`] through [`NATIVE_URLS`].
fn instantiate(parsed: &ParsedUrl) -> Option<Box<dyn Notify>> {
  let factory = registry().get(&parsed.schema)?;
  factory(parsed)
}

/// A service's own webhook URL format (upstream `parse_native_url()`).
pub struct NativeUrl {
  /// Schema of the plugin the URL converts to; the rule is skipped when
  /// that plugin is not registered.
  pub schema: &'static str,
  /// Convert a matching URL to the plugin URL, or `None` when it does not
  /// match.
  pub convert: fn(&str) -> Option<String>,
}

/// Native URL rules, one per upstream plugin with a `parse_native_url()`,
/// in upstream's plugin order. Each regex is copied from upstream; append
/// an entry (and a test) when porting another plugin that has one.
pub static NATIVE_URLS: &[NativeUrl] = &[
  NativeUrl { schema: "slack", convert: native::slack },
  NativeUrl { schema: "lametric", convert: native::lametric },
  NativeUrl { schema: "mmost", convert: native::mattermost },
  NativeUrl { schema: "pushplus", convert: native::pushplus },
  NativeUrl { schema: "wpush", convert: native::wpush },
  NativeUrl { schema: "notica", convert: native::notica },
  NativeUrl { schema: "discord", convert: native::discord },
  NativeUrl { schema: "squadcast", convert: native::squadcast },
  NativeUrl { schema: "zoom", convert: native::zoom },
  NativeUrl { schema: "guilded", convert: native::guilded },
  NativeUrl { schema: "gchat", convert: native::google_chat },
  NativeUrl { schema: "wxteams", convert: native::webex_teams },
  NativeUrl { schema: "fluxer", convert: native::fluxer },
  NativeUrl { schema: "spike", convert: native::spike },
  NativeUrl { schema: "splunk", convert: native::splunk },
  NativeUrl { schema: "ntfys", convert: native::ntfy },
  NativeUrl { schema: "incidentio", convert: native::incidentio },
  NativeUrl { schema: "notifico", convert: native::notifico },
  NativeUrl { schema: "ifttt", convert: native::ifttt },
  NativeUrl { schema: "ryver", convert: native::ryver },
  NativeUrl { schema: "apprise", convert: native::apprise_api },
  NativeUrl { schema: "wecombot", convert: native::wecombot },
  NativeUrl { schema: "qq", convert: native::qq },
  NativeUrl { schema: "youlmk", convert: native::youlmk },
  NativeUrl { schema: "spugpush", convert: native::spugpush },
  NativeUrl { schema: "stackfield", convert: native::stackfield },
  NativeUrl { schema: "matrixs", convert: native::matrix },
  NativeUrl { schema: "kook", convert: native::kook },
  NativeUrl { schema: "flock", convert: native::flock },
  NativeUrl { schema: "46elks", convert: native::fortysixelks },
  NativeUrl { schema: "cliq", convert: native::cliq },
  NativeUrl { schema: "lark", convert: native::lark },
  NativeUrl { schema: "workflow", convert: native::workflows },
  NativeUrl { schema: "lfg", convert: native::lfgalarm },
];

/// The `parse_native_url()` conversions. Each returns the plugin URL that
/// upstream builds before calling the plugin's own `parse_url()`.
mod native {
  use std::sync::OnceLock;

  use regex_lite::{Captures, Regex};

  use crate::utils::parse::{parse_qsd, quote, unquote};

  /// Compile a pattern once.
  fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid native URL pattern"))
  }

  /// A capture group's text, or "" when it did not take part.
  fn group<'a>(c: &'a Captures<'_>, name: &str) -> &'a str {
    c.name(name).map(|m| m.as_str()).unwrap_or("")
  }

  /// Append `key=value` to a `?...` params string (or start one).
  fn with_param(params: &str, kv: &str) -> String {
    if params.is_empty() { format!("?{}", kv) } else { format!("{}&{}", params, kv) }
  }

  macro_rules! pattern {
    ($p:expr) => {{
      static RE: OnceLock<Regex> = OnceLock::new();
      re(&RE, $p)
    }};
  }

  pub fn slack(url: &str) -> Option<String> {
    let wf = pattern!(r"(?i)^https?://hooks\.slack\.com/(?P<wf_type>workflows|triggers)/(?P<path>[A-Z0-9][A-Z0-9/_-]+)/?(?P<params>\?.+)?$");
    if let Some(c) = wf.captures(url) {
      let mode = if group(&c, "wf_type").eq_ignore_ascii_case("triggers") { "trigger" } else { "workflow" };
      let params = group(&c, "params");
      let sep = if params.is_empty() { "?" } else { "&" };
      return Some(format!("slack://{}/{}{}mode={}", group(&c, "path").trim_matches('/'), params, sep, mode));
    }
    let c = pattern!(
      r"(?i)^https?://(?P<host>hooks\.slack(?P<gov>-gov)?\.com)/services/(?P<token_a>[A-Z0-9]+)/(?P<token_b>[A-Z0-9]+)/(?P<token_c>[A-Z0-9]+)/?(?P<params>\?.+)?$"
    )
    .captures(url)?;
    let mut params = group(&c, "params").to_string();
    if c.name("gov").is_some() {
      // Upstream replaces the params with "&mode=gov-hook" here, which
      // then reads as a channel and fails; keep them instead.
      params = with_param(&params, "mode=gov-hook");
    }
    Some(format!("slack://{}/{}/{}/{}", group(&c, "token_a"), group(&c, "token_b"), group(&c, "token_c"), params))
  }

  pub fn lametric(url: &str) -> Option<String> {
    let c = pattern!(
      r"(?i)^http(?P<secure>s)?://(?P<host>[^/]+)/api/(?P<api_ver>v[1-9]*[0-9]+)/dev/widget/update/com\.lametric\.(?P<app_id>[0-9a-z.-]{1,64})(/(?P<app_ver>[1-9][0-9]*))?/?(?P<params>\?.+)?$"
    )
    .captures(url)?;
    let schema = if c.name("secure").is_some() { "lametrics" } else { "lametric" };
    let app_ver = c.name("app_ver").map(|m| format!("/{}", m.as_str())).unwrap_or_default();
    Some(format!("{}://{}{}/{}", schema, group(&c, "app_id"), app_ver, group(&c, "params")))
  }

  pub fn mattermost(url: &str) -> Option<String> {
    let c =
      pattern!(r"(?i)^http(?P<secure>s?)://(?P<host>mattermost\.[A-Z0-9_.-]+)(:(?P<port>[1-9][0-9]{0,5}))?/hooks/(?P<token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$")
        .captures(url)?;
    // Upstream compares the port with a "default" taken from that same
    // port, so the port is always dropped; kept as upstream does it.
    let schema = if group(&c, "secure").is_empty() { "mmost" } else { "mmosts" };
    Some(format!("{}://{}/{}/{}", schema, group(&c, "host"), group(&c, "token"), group(&c, "params")))
  }

  pub fn pushplus(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://www\.pushplus\.plus/send(?:\?(?P<params>[^#]+))?$").captures(url)?;
    let params = group(&c, "params");
    let tok = pattern!(r"(?i)(?:(?:^|&))token=(?P<token>[a-z0-9_-]+)").captures(params)?;
    Some(format!("pushplus://{}/?{}", group(&tok, "token"), params))
  }

  pub fn wpush(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://api\.wpush\.cn/api/v1/send(?:\?(?P<params>[^#]+))?$").captures(url)?;
    let params = group(&c, "params");
    let key = pattern!(r"(?i)(?:(?:^|&))apikey=(?P<apikey>[^&]+)").captures(params)?;
    Some(format!("wpush://{}/?{}", unquote(group(&key, "apikey")), params))
  }

  pub fn notica(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://notica\.us/?\??(?P<token>[^&]+)([&\s]*(?P<params>.+))?$").captures(url)?;
    let params = c.name("params").map(|m| format!("?{}", m.as_str())).unwrap_or_default();
    Some(format!("notica://{}/{}", group(&c, "token"), params))
  }

  pub fn discord(url: &str) -> Option<String> {
    let c =
      pattern!(r"(?i)^https?://discord(app)?\.com/api/webhooks/(?P<webhook_id>[0-9]+)/(?P<webhook_token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("discord://{}/{}/{}", group(&c, "webhook_id"), group(&c, "webhook_token"), group(&c, "params")))
  }

  pub fn squadcast(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://api\.(?P<eu>eu\.)?squadcast\.com/v2/incidents/api/(?P<key>[A-Z0-9_-]+)/?(?P<params>\?.+)?$").captures(url)?;
    let mut params = group(&c, "params").to_string();
    if c.name("eu").is_some() {
      // Upstream sets region_name after parsing; ?region= maps to it.
      params = with_param(&params, "region=eu");
    }
    Some(format!("squadcast://{}/{}", group(&c, "key"), params))
  }

  pub fn zoom(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://inbots\.zoom\.us/incoming/hook/(?P<webhook_id>[A-Za-z0-9_-]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("zoom://{}/{}", group(&c, "webhook_id"), group(&c, "params")))
  }

  pub fn guilded(url: &str) -> Option<String> {
    let c =
      pattern!(r"(?i)^https?://(media\.)?guilded\.gg/webhooks/(?P<webhook_id>[-0-9a-f]+)/(?P<webhook_token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("guilded://{}/{}/{}", group(&c, "webhook_id"), group(&c, "webhook_token"), group(&c, "params")))
  }

  pub fn google_chat(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https://chat\.googleapis\.com/v1/spaces/(?P<workspace>[A-Z0-9_-]+)/messages/*(?P<params>.+)$").captures(url)?;
    Some(format!("gchat://{}/{}", group(&c, "workspace"), group(&c, "params")))
  }

  pub fn webex_teams(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://(api\.ciscospark\.com|webexapis\.com)/v[1-9][0-9]*/webhooks/incoming/(?P<webhook_token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$")
      .captures(url)?;
    Some(format!("wxteams://{}/{}", group(&c, "webhook_token"), group(&c, "params")))
  }

  pub fn fluxer(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://(api\.)?fluxer\.app/(?:(?:v[0-9]+/)?webhooks)/(?P<webhook_id>[0-9]+)/(?P<webhook_token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$")
      .captures(url)?;
    Some(format!("fluxers://{}/{}/{}", group(&c, "webhook_id"), group(&c, "webhook_token"), group(&c, "params")))
  }

  pub fn spike(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https://api\.spike\.sh/v1/alerts/([a-z0-9]{32})$").captures(url)?;
    Some(format!("spike://{}", &c[1]))
  }

  pub fn splunk(url: &str) -> Option<String> {
    let c = pattern!(
      r"(?i)^https?://alert\.victorops\.com/integrations/generic/(?P<version>[0-9]+)/alert/(?P<apikey>[0-9a-z_-]+)(/(?P<routing_key>[^?/]+))(/(?P<entity_id>[^?]+))?/*(?P<params>\?.+)?$"
    )
    .captures(url)?;
    Some(format!("splunk://{}@{}/{}{}", group(&c, "routing_key"), group(&c, "apikey"), group(&c, "entity_id"), group(&c, "params")))
  }

  pub fn ntfy(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^(http|ntfy)s?://ntfy\.sh(?P<topics>/[^?]+)?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("ntfys://{}{}", group(&c, "topics"), with_param(group(&c, "params"), "mode=cloud")))
  }

  pub fn incidentio(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://api\.incident\.io/v2/alert_events/http/(?P<source>[A-Z0-9]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("incidentio://{}/{}", group(&c, "source"), group(&c, "params")))
  }

  pub fn notifico(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://n\.tkte\.ch/h/(?P<proj>[0-9]+)/(?P<hook>[A-Z0-9]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("notifico://{}/{}/{}", group(&c, "proj"), group(&c, "hook"), group(&c, "params")))
  }

  pub fn ifttt(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://maker\.ifttt\.com/use/(?P<webhook_id>[A-Z0-9_-]+)((?P<events>(/[A-Z0-9_-]+)+))?/?(?P<params>\?.+)?$").captures(url)?;
    let events = c.name("events").map(|m| format!("@{}", m.as_str())).unwrap_or_default();
    Some(format!("ifttt://{}{}{}", group(&c, "webhook_id"), events, group(&c, "params")))
  }

  pub fn ryver(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://(?P<org>[A-Z0-9_-]+)\.ryver\.com/application/webhook/(?P<webhook_token>[A-Z0-9]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("ryver://{}/{}/{}", group(&c, "org"), group(&c, "webhook_token"), group(&c, "params")))
  }

  pub fn apprise_api(url: &str) -> Option<String> {
    let c = pattern!(
      r"(?i)^http(?P<secure>s?)://(?P<hostname>[A-Z0-9._-]+)(:(?P<port>[0-9]+))?(?P<path>/[^?]+?)?/notify/(?P<token>[A-Z0-9_-]{1,32})/?(?P<params>\?.+)?$"
    )
    .captures(url)?;
    let schema = if group(&c, "secure").is_empty() { "apprise" } else { "apprises" };
    let port = c.name("port").map(|m| format!(":{}", m.as_str())).unwrap_or_default();
    let mut params = group(&c, "params").to_string();
    // Native URLs default to the v1 API unless ?version= says otherwise.
    let has_version = parse_qsd(params.trim_start_matches('?'), false).qsd.iter().any(|(k, v)| k.eq_ignore_ascii_case("version") && !v.is_empty());
    if !has_version {
      params = with_param(&params, "version=1");
    }
    Some(format!("{}://{}{}{}/{}/{}", schema, group(&c, "hostname"), port, group(&c, "path"), group(&c, "token"), params))
  }

  pub fn wecombot(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://qyapi\.weixin\.qq\.com/cgi-bin/webhook/send/?\?key=(?P<key>[A-Z0-9_-]+)/?&?(?P<params>.+)?$").captures(url)?;
    let params = c.name("params").map(|m| format!("?{}", m.as_str())).unwrap_or_default();
    Some(format!("wecombot://{}{}", group(&c, "key"), params))
  }

  pub fn qq(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https://qmsg\.zendee\.cn/send/([a-z0-9]+)$").captures(url)?;
    Some(format!("qq://{}", &c[1]))
  }

  pub fn youlmk(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://(www\.)?youlmk\.com/k/(?P<key>k_[a-z0-9]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("youlmk://{}/{}", group(&c, "key"), group(&c, "params")))
  }

  pub fn spugpush(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https://push\.spug\.dev/send/([a-z0-9_-]+)$").captures(url)?;
    Some(format!("spugpush://{}", &c[1]))
  }

  pub fn stackfield(url: &str) -> Option<String> {
    let c =
      pattern!(r"(?i)^https?://(?:www\.)?stackfield\.com/apiwh/(?P<token>[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/?(?P<params>\?.+)?$")
        .captures(url)?;
    Some(format!("stackfield://{}/{}", group(&c, "token"), group(&c, "params")))
  }

  pub fn matrix(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://webhooks\.t2bot\.io/api/v[0-9]+/matrix/hook/(?P<webhook_token>[A-Z0-9_-]+)/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("matrixs://{}/{}", group(&c, "webhook_token"), with_param(group(&c, "params"), "mode=t2bot")))
  }

  pub fn kook(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://www\.kookapp\.cn/api/v3/incoming/(?P<key>[A-Za-z0-9_=+-]+)/?(?P<params>\?.+)?$").captures(url)?;
    let extra = c.name("params").map(|m| format!("&{}", m.as_str().trim_start_matches('?'))).unwrap_or_default();
    Some(format!("kook://{}/?mode=webhook{}", group(&c, "key"), extra))
  }

  pub fn flock(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://api\.flock\.com/hooks/sendMessage/(?P<token>[a-z0-9-]{24})/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("flock://{}/{}", group(&c, "token"), group(&c, "params")))
  }

  pub fn fortysixelks(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://(?P<credentials>[^@]+)@api\.46elks\.com/a1/sms/?(?P<params>\?.+)$").captures(url)?;
    Some(format!("46elks://{}@/{}", group(&c, "credentials"), group(&c, "params")))
  }

  pub fn cliq(url: &str) -> Option<String> {
    let c = pattern!(
      r"(?i)^https?://(?P<domain>cliq\.zoho(cloud)?(\.[a-z]+){1,2})/+(company/(?P<company>[A-Z0-9_-]+)/+)?api/v2/+(?P<path>channelsbyname|bots)/(?P<name>[^/?]+)/+message/*\?(?P<params>(.*&)?zapikey=(?P<token>[^&]+).*)$"
    )
    .captures(url)?;
    let region = match group(&c, "domain").to_lowercase().as_str() {
      "cliq.zoho.com" => "us",
      "cliq.zoho.eu" => "eu",
      "cliq.zoho.in" => "in",
      "cliq.zoho.com.au" => "au",
      "cliq.zoho.com.cn" => "cn",
      "cliq.zoho.jp" => "jp",
      "cliq.zoho.sa" => "sa",
      "cliq.zoho.uk" => "uk",
      "cliq.zohocloud.ca" => "ca",
      _ => return None,
    };
    let prefix = if group(&c, "path").eq_ignore_ascii_case("bots") { "@" } else { "#" };
    let target = quote(&format!("{}{}", prefix, group(&c, "name")), "@");
    let company = c.name("company").map(|m| format!("&company={}", m.as_str())).unwrap_or_default();
    Some(format!("cliq://{}/{}?region={}{}", group(&c, "token"), target, region, company))
  }

  pub fn lark(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https://open\.larksuite\.com/open-apis/bot/v2/hook/([\w-]+)$").captures(url)?;
    Some(format!("lark://{}", &c[1]))
  }

  pub fn workflows(url: &str) -> Option<String> {
    let c = pattern!(
      r"(?i)^https?://(?P<host>[A-Z0-9_.-]+)(?P<port>:[1-9][0-9]{0,5})?(?P<power_automate>/powerautomate/automations/direct(?:/cu/(?P<routing_id>[0-9]+))?)?/workflows/(?P<workflow>[A-Z0-9_-]+)/triggers/manual/paths/invoke/?(?P<params>\?.+)$"
    )
    .captures(url)?;
    let mut converted = format!("workflow://{}{}/{}/{}", group(&c, "host"), group(&c, "port"), group(&c, "workflow"), group(&c, "params"));
    if c.name("power_automate").is_some() {
      converted.push_str("&pa=yes");
    }
    if let Some(id) = c.name("routing_id") {
      // Upstream sets routing_id after parsing; ?route= maps to it.
      converted.push_str(&format!("&route={}", id.as_str()));
    }
    Some(converted)
  }

  pub fn lfgalarm(url: &str) -> Option<String> {
    let c = pattern!(r"(?i)^https?://api\.lfg\.gm/horn/(?P<token>[a-z0-9]{32})/?(?P<params>\?.+)?$").captures(url)?;
    Some(format!("lfg://{}/{}", group(&c, "token"), group(&c, "params")))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The plugin URL upstream's `parse_native_url()` builds for each sample,
  /// captured by wrapping every plugin's `parse_url()` (Apprise 2.1.0).
  /// Where upstream sets a constructor argument after parsing (squadcast
  /// `region_name`, apprise `version`, workflows `routing_id`), the Rust
  /// rule carries it as the matching query argument instead; those rows
  /// are marked.
  const NATIVE_VECTORS: &[(&str, Option<&str>)] = &[
    (r#"https://hooks.slack.com/services/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ"#, Some(r#"slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/"#)),
    (
      r#"https://hooks.slack.com/services/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?footer=no"#,
      Some(r#"slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?footer=no"#),
    ),
    // Upstream builds `.../&mode=gov-hook` here, dropping ?footer=no and
    // failing on the bogus channel; the Rust rule keeps the arguments.
    (
      r#"https://hooks.slack-gov.com/services/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ?footer=no"#,
      Some(r#"slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?footer=no&mode=gov-hook"#),
    ),
    (r#"https://hooks.slack.com/workflows/T0123/A0456/123/abcd"#, Some(r#"slack://T0123/A0456/123/abcd/?mode=workflow"#)),
    (r#"https://hooks.slack.com/triggers/T0123/A0456/123/abcd/?x=1"#, Some(r#"slack://T0123/A0456/123/abcd/?x=1&mode=trigger"#)),
    (r#"https://developer.lametric.com/api/v1/dev/widget/update/com.lametric.ABCD123/1?token=abcd"#, Some(r#"lametrics://ABCD123/1/?token=abcd"#)),
    (r#"http://developer.lametric.com/api/v2/dev/widget/update/com.lametric.abcd/"#, Some(r#"lametric://abcd/"#)),
    (r#"https://mattermost.example.com/hooks/3ccdd113474722377935511fc85d3dd4"#, Some(r#"mmosts://mattermost.example.com/3ccdd113474722377935511fc85d3dd4/"#)),
    (r#"http://mattermost.example.com:8065/hooks/abc_123?channel=x"#, Some(r#"mmost://mattermost.example.com/abc_123/?channel=x"#)),
    (r#"https://mattermost.example.com:443/hooks/abc"#, Some(r#"mmosts://mattermost.example.com/abc/"#)),
    (r#"https://example.com/hooks/abc"#, None),
    (r#"https://www.pushplus.plus/send?token=abc123&title=x"#, Some(r#"pushplus://abc123/?token=abc123&title=x"#)),
    (r#"https://api.wpush.cn/api/v1/send?apikey=abc%2Bd&channel=wechat"#, Some(r#"wpush://abc+d/?apikey=abc%2Bd&channel=wechat"#)),
    (r#"https://notica.us/?abc123"#, Some(r#"notica://abc123/"#)),
    (r#"https://notica.us/?abc123&format=html"#, Some(r#"notica://abc123/?format=html"#)),
    (r#"https://discord.com/api/webhooks/0123456789/abcdefghijk_lmnop-qrstuvwxyz"#, Some(r#"discord://0123456789/abcdefghijk_lmnop-qrstuvwxyz/"#)),
    (r#"https://discordapp.com/api/webhooks/0123456789/abc/?footer=yes"#, Some(r#"discord://0123456789/abc/?footer=yes"#)),
    (r#"https://api.squadcast.com/v2/incidents/api/abc123"#, Some(r#"squadcast://abc123/"#)),
    (r#"https://api.eu.squadcast.com/v2/incidents/api/abc123?priority=P1"#, Some(r#"squadcast://abc123/?priority=P1&region=eu"#)), // region_name
    (r#"https://inbots.zoom.us/incoming/hook/abcDEF_12-3?format=fields"#, Some(r#"zoom://abcDEF_12-3/?format=fields"#)),
    (
      r#"https://media.guilded.gg/webhooks/b974c1d8-1a2b-3c4d-5e6f-7a8b9c0d1e2f/abc_DEF-1"#,
      Some(r#"guilded://b974c1d8-1a2b-3c4d-5e6f-7a8b9c0d1e2f/abc_DEF-1/"#),
    ),
    (r#"https://chat.googleapis.com/v1/spaces/myworkspace/messages?key=mykey&token=mytoken"#, Some(r#"gchat://myworkspace/?key=mykey&token=mytoken"#)),
    (r#"https://webexapis.com/v1/webhooks/incoming/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#, Some(r#"wxteams://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"#)),
    (r#"https://api.ciscospark.com/v2/webhooks/incoming/abc?format=markdown"#, Some(r#"wxteams://abc/?format=markdown"#)),
    (r#"https://api.fluxer.app/v1/webhooks/0123456789/abcdef"#, Some(r#"fluxers://0123456789/abcdef/"#)),
    (r#"https://fluxer.app/webhooks/0123456789/abcdef/?tts=yes"#, Some(r#"fluxers://0123456789/abcdef/?tts=yes"#)),
    (r#"https://api.spike.sh/v1/alerts/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#, Some(r#"spike://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#)),
    (r#"https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key"#, Some(r#"splunk://routing_key@apikey/"#)),
    (
      r#"https://alert.victorops.com/integrations/generic/20131114/alert/apikey/routing_key/entity/id?action=warning"#,
      Some(r#"splunk://routing_key@apikey/entity/id?action=warning"#),
    ),
    (r#"https://ntfy.sh/topic1/topic2"#, Some(r#"ntfys:///topic1/topic2?mode=cloud"#)),
    (r#"https://ntfy.sh/?priority=max"#, None),
    (r#"ntfys://ntfy.sh/topic"#, Some(r#"ntfys:///topic?mode=cloud"#)),
    (r#"https://api.incident.io/v2/alert_events/http/SRC123?x=1"#, Some(r#"incidentio://SRC123/?x=1"#)),
    (r#"https://n.tkte.ch/h/2144/uJmKaBW9WFk42miB146ci3Kj"#, Some(r#"notifico://2144/uJmKaBW9WFk42miB146ci3Kj/"#)),
    (r#"https://maker.ifttt.com/use/WebHookID/EventID/"#, Some(r#"ifttt://WebHookID@/EventID"#)),
    (r#"https://maker.ifttt.com/use/WebHookID/Event1/Event2?+key=value"#, Some(r#"ifttt://WebHookID@/Event1/Event2?+key=value"#)),
    (r#"https://maker.ifttt.com/use/WebHookID"#, Some(r#"ifttt://WebHookID"#)),
    (r#"https://apprise.ryver.com/application/webhook/ckhrjW8w672m6HG"#, Some(r#"ryver://apprise/ckhrjW8w672m6HG/"#)),
    (r#"https://localhost:8000/notify/abc123"#, Some(r#"apprises://localhost:8000/abc123/?version=1"#)), // version
    (r#"http://user:pass@localhost/notify/abc123?version=2"#, None),
    (r#"http://example.com/path/to/notify/abc-123/?tags=all"#, Some(r#"apprise://example.com/path/to/abc-123/?tags=all&version=1"#)), // version
    (r#"https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc123"#, Some(r#"wecombot://abc123"#)),
    (r#"https://qyapi.weixin.qq.com/cgi-bin/webhook/send/?key=abc123&format=markdown"#, Some(r#"wecombot://abc123?format=markdown"#)),
    (r#"https://qmsg.zendee.cn/send/abc123"#, Some(r#"qq://abc123"#)),
    (r#"https://youlmk.com/k/k_abc123?x=1"#, Some(r#"youlmk://k_abc123/?x=1"#)),
    (r#"https://push.spug.dev/send/abc-123"#, Some(r#"spugpush://abc-123"#)),
    (r#"https://www.stackfield.com/apiwh/12345678-1234-1234-1234-123456789abc"#, Some(r#"stackfield://12345678-1234-1234-1234-123456789abc/"#)),
    (r#"https://webhooks.t2bot.io/api/v1/matrix/hook/abc123"#, Some(r#"matrixs://abc123/?mode=t2bot"#)),
    (r#"https://webhooks.t2bot.io/api/v1/matrix/hook/abc123?format=html"#, Some(r#"matrixs://abc123/?format=html&mode=t2bot"#)),
    (r#"https://www.kookapp.cn/api/v3/incoming/abc=+-_123?x=1"#, Some(r#"kook://abc=+-_123/?mode=webhook&x=1"#)),
    (r#"https://api.flock.com/hooks/sendMessage/aaaaaaaaaaaaaaaaaaaaaaaa"#, Some(r#"flock://aaaaaaaaaaaaaaaaaaaaaaaa/"#)),
    (r#"https://user:pass@api.46elks.com/a1/sms?to=+46701234567&from=me"#, Some(r#"46elks://user:pass@/?to=+46701234567&from=me"#)),
    (r#"https://cliq.zoho.eu/company/123/api/v2/channelsbyname/alerts/message?zapikey=TOKEN"#, Some(r#"cliq://TOKEN/%23alerts?region=eu&company=123"#)),
    (r#"https://cliq.zoho.com/api/v2/bots/mybot/message?a=b&zapikey=TOKEN&c=d"#, Some(r#"cliq://TOKEN/@mybot?region=us"#)),
    (r#"https://cliq.zoho.xx/api/v2/bots/mybot/message?zapikey=TOKEN"#, None),
    (r#"https://open.larksuite.com/open-apis/bot/v2/hook/abc-123"#, Some(r#"lark://abc-123"#)),
    (
      r#"https://server.azure.com:443/workflows/643e69f83c8944/triggers/manual/paths/invoke?api-version=2016-06-01&sig=abc"#,
      Some(r#"workflow://server.azure.com:443/643e69f83c8944/?api-version=2016-06-01&sig=abc"#),
    ),
    (
      r#"https://prod.powerautomate.com/powerautomate/automations/direct/cu/12345/workflows/abc/triggers/manual/paths/invoke?sig=x"#,
      Some(r#"workflow://prod.powerautomate.com/abc/?sig=x&pa=yes&route=12345"#),
    ), // routing_id
    (r#"https://api.lfg.gm/horn/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#, Some(r#"lfg://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"#)),
    (r#"https://example.com/not/native"#, None),
  ];

  /// Run a URL through the rules the way `parse_service_url` does, but
  /// without skipping rules for plugins the Rust port does not have yet.
  fn convert(url: &str) -> Option<String> {
    let url = escape_path_hash(url);
    NATIVE_URLS.iter().find_map(|n| (n.convert)(&url))
  }

  #[test]
  fn test_native_url_conversions_match_upstream() {
    for (url, expected) in NATIVE_VECTORS {
      assert_eq!(convert(url).as_deref(), *expected, "native conversion of {}", url);
    }
  }

  #[test]
  fn test_every_native_rule_has_a_vector() {
    // Each rule must convert at least one sample, so a typo in a pattern
    // can not hide behind another rule.
    for native in NATIVE_URLS {
      let hit = NATIVE_VECTORS.iter().any(|(url, _)| (native.convert)(&escape_path_hash(url)).is_some());
      assert!(hit, "no sample URL exercises the {} rule", native.schema);
    }
  }

  #[test]
  fn test_mattermost_needs_mattermost_host() {
    // Before this port any http(s) URL with "hooks" in its path went to
    // Mattermost; upstream only accepts a mattermost.* host.
    assert!(parse_service_url("https://example.com/hooks/abcdef").is_none());
    assert!(parse_service_url("https://example.com/webhooks/abcdef").is_none());
    let p = parse_service_url("https://mattermost.example.com/hooks/abcdef").unwrap();
    assert_eq!(p.schema, "mmosts");
    assert_eq!(p.host.as_deref(), Some("mattermost.example.com"));
    assert_eq!(p.path_parts, vec!["abcdef"]);
  }

  #[test]
  fn test_native_url_loads_plugin() {
    let svc = from_url("https://discord.com/api/webhooks/0123456789/abcdefghijk_lmnop-qrstuvwxyz?tts=yes").unwrap();
    assert_eq!(svc.service_name(), "Discord");
    let p = parse_service_url("https://discord.com/api/webhooks/0123456789/abcdefghijk_lmnop-qrstuvwxyz?tts=yes").unwrap();
    assert_eq!(p.schema, "discord");
    assert_eq!(p.host.as_deref(), Some("0123456789"));
    assert_eq!(p.get("tts"), Some("yes"));
  }

  #[test]
  fn test_registered_schema_is_parsed_directly() {
    // A schema that belongs to a plugin never goes through native rules.
    let p = parse_service_url("ntfy://ntfy.sh/topic").unwrap();
    assert_eq!(p.schema, "ntfy");
    assert_eq!(p.host.as_deref(), Some("ntfy.sh"));
    // A plugin that rejects its own URL does not fall back to native rules.
    assert!(from_url("discord://").is_none());
  }

  #[test]
  fn test_url_schema() {
    assert_eq!(url_schema("  JSON://host").as_deref(), Some("json"));
    assert_eq!(url_schema("json://host\n").as_deref(), Some("json"));
    assert!(url_schema("json://ho\nst").is_none());
    assert!(url_schema("json:/host").is_none());
    assert!(url_schema(&format!("{}://host", "a".repeat(33))).is_none());
    assert!(url_schema("://host").is_none());
  }

  #[test]
  fn test_unsupported_urls() {
    assert!(parse_service_url("unknown://host").is_none());
    assert!(parse_service_url("not a url").is_none());
    assert!(parse_service_url("https://example.com/not/native").is_none());
  }

  #[test]
  fn test_hash_after_slash_is_literal() {
    let p = parse_service_url("json://localhost/#channel").unwrap();
    assert_eq!(p.path_parts, vec!["#channel"]);
  }
}
