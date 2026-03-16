use std::collections::HashMap;
use std::sync::OnceLock;

use super::Notify;
use crate::utils::parse::ParsedUrl;

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
  collect!(apprise_api::AppriseApi::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(aprs::Aprs::static_details());
  collect!(bark::Bark::static_details());
  collect!(bluesky::BlueSky::static_details());
  collect!(brevo::Brevo::static_details());
  collect!(bulksms::BulkSms::static_details());
  collect!(bulkvs::BulkVs::static_details());
  collect!(burstsms::BurstSms::static_details());
  collect!(chanify::Chanify::static_details());
  collect!(clickatell::Clickatell::static_details());
  collect!(clicksend::ClickSend::static_details());
  collect!(d7networks::D7Networks::static_details());
  collect!(dapnet::Dapnet::static_details());
  collect!(dingtalk::DingTalk::static_details());
  collect!(discord::Discord::static_details());
  collect!(dot::Dot::static_details());
  collect!(emby::Emby::static_details());
  collect!(enigma2::Enigma2::static_details());
  collect!(fcm::Fcm::static_details());
  collect!(feishu::FeiShu::static_details());
  collect!(flock::Flock::static_details());
  collect!(fluxer::Fluxer::static_details());
  collect!(fortysixelks::FortySixElks::static_details());
  collect!(freemobile::FreeMobile::static_details());
  collect!(google_chat::GoogleChat::static_details());
  collect!(gotify::Gotify::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(growl::Growl::static_details());
  collect!(guilded::Guilded::static_details());
  collect!(home_assistant::HomeAssistant::static_details());
  collect!(httpsms::HttpSms::static_details());
  collect!(ifttt::Ifttt::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(irc::Irc::static_details());
  collect!(jellyfin::Jellyfin::static_details());
  collect!(join::Join::static_details());
  collect!(custom_json::Json::static_details());
  collect!(custom_form::Form::static_details());
  collect!(custom_xml::Xml::static_details());
  collect!(kavenegar::Kavenegar::static_details());
  collect!(kumulos::Kumulos::static_details());
  collect!(lametric::LaMetric::static_details());
  collect!(lark::Lark::static_details());
  collect!(line::Line::static_details());
  collect!(mailgun::Mailgun::static_details());
  collect!(mastodon::Mastodon::static_details());
  collect!(matrix::Matrix::static_details());
  collect!(mattermost::Mattermost::static_details());
  collect!(messagebird::MessageBird::static_details());
  collect!(misskey::Misskey::static_details());
  #[cfg(all(feature = "mqtt", not(target_arch = "wasm32")))]
  collect!(mqtt::Mqtt::static_details());
  collect!(msg91::Msg91::static_details());
  collect!(msteams::MsTeams::static_details());
  collect!(nextcloud::Nextcloud::static_details());
  collect!(nextcloudtalk::NextcloudTalk::static_details());
  collect!(notica::Notica::static_details());
  collect!(notifiarr::Notifiarr::static_details());
  collect!(notificationapi::NotificationApi::static_details());
  collect!(notifico::Notifico::static_details());
  collect!(ntfy::Ntfy::static_details());
  collect!(office365::Office365::static_details());
  collect!(one_signal::OneSignal::static_details());
  collect!(opsgenie::OpsGenie::static_details());
  collect!(pagerduty::PagerDuty::static_details());
  collect!(pagertree::PagerTree::static_details());
  collect!(parseplatform::ParsePlatform::static_details());
  collect!(plivo::Plivo::static_details());
  collect!(popcorn_notify::PopcornNotify::static_details());
  collect!(prowl::Prowl::static_details());
  collect!(pushbullet::Pushbullet::static_details());
  collect!(pushdeer::PushDeer::static_details());
  collect!(pushed::Pushed::static_details());
  collect!(pushjet::PushJet::static_details());
  collect!(pushme::PushMe::static_details());
  collect!(pushover::Pushover::static_details());
  collect!(pushplus::PushPlus::static_details());
  collect!(pushsafer::Pushsafer::static_details());
  collect!(pushy::Pushy::static_details());
  collect!(qq::Qq::static_details());
  collect!(reddit::Reddit::static_details());
  collect!(resend::Resend::static_details());
  collect!(revolt::Revolt::static_details());
  collect!(rocketchat::RocketChat::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(rsyslog::RSyslog::static_details());
  collect!(ryver::Ryver::static_details());
  collect!(sendgrid::SendGrid::static_details());
  collect!(sendpulse::SendPulse::static_details());
  collect!(serverchan::ServerChan::static_details());
  collect!(ses::Ses::static_details());
  collect!(seven::Seven::static_details());
  collect!(sfr::Sfr::static_details());
  collect!(signal_api::SignalApi::static_details());
  collect!(signl4::Signl4::static_details());
  collect!(simplepush::SimplePush::static_details());
  collect!(sinch::Sinch::static_details());
  collect!(slack::Slack::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(smpp::Smpp::static_details());
  collect!(smseagle::SmsEagle::static_details());
  collect!(smsmanager::SmsManager::static_details());
  collect!(smtp2go::Smtp2Go::static_details());
  collect!(sns::Sns::static_details());
  collect!(sparkpost::SparkPost::static_details());
  collect!(spike::Spike::static_details());
  collect!(splunk::Splunk::static_details());
  collect!(spugpush::SpugPush::static_details());
  collect!(streamlabs::Streamlabs::static_details());
  collect!(synology::Synology::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(syslog::Syslog::static_details());
  collect!(techuluspush::TechulusPush::static_details());
  collect!(telegram::Telegram::static_details());
  collect!(threema::Threema::static_details());
  collect!(twilio::Twilio::static_details());
  collect!(twist::Twist::static_details());
  collect!(twitter::Twitter::static_details());
  collect!(viber::Viber::static_details());
  collect!(voipms::VoipMs::static_details());
  collect!(vapid::Vapid::static_details());
  collect!(vonage::Vonage::static_details());
  collect!(webexteams::WebexTeams::static_details());
  collect!(wecombot::WeComBot::static_details());
  collect!(whatsapp::WhatsApp::static_details());
  collect!(workflows::Workflows::static_details());
  collect!(wxpusher::WxPusher::static_details());
  collect!(xbmc::Xbmc::static_details());
  #[cfg(not(target_arch = "wasm32"))]
  collect!(xmpp::Xmpp::static_details());
  collect!(zulip::Zulip::static_details());
  #[cfg(all(feature = "email", not(target_arch = "wasm32")))]
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
  reg!(apprise_api::AppriseApi::from_url, "apprise", "apprises");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(aprs::Aprs::from_url, "aprs");
  reg!(bark::Bark::from_url, "bark", "barks");
  reg!(bluesky::BlueSky::from_url, "bsky", "bluesky");
  reg!(brevo::Brevo::from_url, "brevo");
  reg!(bulksms::BulkSms::from_url, "bulksms");
  reg!(bulkvs::BulkVs::from_url, "bulkvs");
  reg!(burstsms::BurstSms::from_url, "burstsms");
  reg!(chanify::Chanify::from_url, "chanify");
  reg!(clickatell::Clickatell::from_url, "clickatell");
  reg!(clicksend::ClickSend::from_url, "clicksend");
  reg!(d7networks::D7Networks::from_url, "d7sms");
  reg!(dapnet::Dapnet::from_url, "dapnet");
  reg!(dingtalk::DingTalk::from_url, "dingtalk");
  reg!(discord::Discord::from_url, "discord");
  reg!(dot::Dot::from_url, "dot");
  reg!(emby::Emby::from_url, "emby", "embys");
  reg!(enigma2::Enigma2::from_url, "enigma2", "enigma2s");
  reg!(fcm::Fcm::from_url, "fcm");
  reg!(feishu::FeiShu::from_url, "feishu");
  reg!(flock::Flock::from_url, "flock");
  reg!(fluxer::Fluxer::from_url, "fluxer", "fluxers");
  reg!(fortysixelks::FortySixElks::from_url, "46elks", "elks");
  reg!(freemobile::FreeMobile::from_url, "freemobile");
  reg!(google_chat::GoogleChat::from_url, "gchat");
  reg!(gotify::Gotify::from_url, "gotify", "gotifys");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(growl::Growl::from_url, "growl");
  reg!(guilded::Guilded::from_url, "guilded");
  reg!(home_assistant::HomeAssistant::from_url, "hassio", "hassios");
  reg!(httpsms::HttpSms::from_url, "httpsms");
  reg!(ifttt::Ifttt::from_url, "ifttt");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(irc::Irc::from_url, "irc", "ircs");
  reg!(jellyfin::Jellyfin::from_url, "jellyfin", "jellyfins");
  reg!(join::Join::from_url, "join");
  reg!(custom_json::Json::from_url, "json", "jsons");
  reg!(custom_form::Form::from_url, "form", "forms");
  reg!(custom_xml::Xml::from_url, "xml", "xmls");
  reg!(kavenegar::Kavenegar::from_url, "kavenegar");
  reg!(kumulos::Kumulos::from_url, "kumulos");
  reg!(lametric::LaMetric::from_url, "lametric", "lametrics");
  reg!(lark::Lark::from_url, "lark");
  reg!(line::Line::from_url, "line");
  reg!(mailgun::Mailgun::from_url, "mailgun");
  reg!(mastodon::Mastodon::from_url, "mastodon", "toot", "mastodons", "toots");
  reg!(matrix::Matrix::from_url, "matrix", "matrixs");
  reg!(mattermost::Mattermost::from_url, "mmost", "mmosts");
  reg!(messagebird::MessageBird::from_url, "msgbird");
  reg!(misskey::Misskey::from_url, "misskey", "misskeys");
  #[cfg(all(feature = "mqtt", not(target_arch = "wasm32")))]
  reg!(mqtt::Mqtt::from_url, "mqtt", "mqtts");
  reg!(msg91::Msg91::from_url, "msg91");
  reg!(msteams::MsTeams::from_url, "msteams");
  reg!(nextcloud::Nextcloud::from_url, "ncloud", "nclouds");
  reg!(nextcloudtalk::NextcloudTalk::from_url, "nctalk", "nctalks");
  reg!(notica::Notica::from_url, "notica", "noticas");
  reg!(notifiarr::Notifiarr::from_url, "notifiarr");
  reg!(notificationapi::NotificationApi::from_url, "napi", "notificationapi");
  reg!(notifico::Notifico::from_url, "notifico");
  reg!(ntfy::Ntfy::from_url, "ntfy", "ntfys");
  reg!(office365::Office365::from_url, "o365", "azure");
  reg!(one_signal::OneSignal::from_url, "onesignal");
  reg!(opsgenie::OpsGenie::from_url, "opsgenie");
  reg!(pagerduty::PagerDuty::from_url, "pagerduty");
  reg!(pagertree::PagerTree::from_url, "pagertree");
  reg!(parseplatform::ParsePlatform::from_url, "parsep", "parseps");
  reg!(plivo::Plivo::from_url, "plivo");
  reg!(popcorn_notify::PopcornNotify::from_url, "popcorn");
  reg!(prowl::Prowl::from_url, "prowl");
  reg!(pushbullet::Pushbullet::from_url, "pbul");
  reg!(pushdeer::PushDeer::from_url, "pushdeer", "pushdeers", "push");
  reg!(pushed::Pushed::from_url, "pushed");
  reg!(pushjet::PushJet::from_url, "pjet", "pjets");
  reg!(pushme::PushMe::from_url, "pushme");
  reg!(pushover::Pushover::from_url, "pover");
  reg!(pushplus::PushPlus::from_url, "pushplus");
  reg!(pushsafer::Pushsafer::from_url, "psafer", "psafers");
  reg!(pushy::Pushy::from_url, "pushy");
  reg!(qq::Qq::from_url, "qq");
  reg!(reddit::Reddit::from_url, "reddit");
  reg!(resend::Resend::from_url, "resend");
  reg!(revolt::Revolt::from_url, "revolt");
  reg!(rocketchat::RocketChat::from_url, "rocket", "rockets");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(rsyslog::RSyslog::from_url, "rsyslog");
  reg!(ryver::Ryver::from_url, "ryver");
  reg!(sendgrid::SendGrid::from_url, "sendgrid");
  reg!(sendpulse::SendPulse::from_url, "sendpulse");
  reg!(serverchan::ServerChan::from_url, "schan");
  reg!(ses::Ses::from_url, "ses");
  reg!(seven::Seven::from_url, "seven");
  reg!(sfr::Sfr::from_url, "sfr");
  reg!(signal_api::SignalApi::from_url, "signal", "signals");
  reg!(signl4::Signl4::from_url, "signl4");
  reg!(simplepush::SimplePush::from_url, "spush");
  reg!(sinch::Sinch::from_url, "sinch");
  reg!(slack::Slack::from_url, "slack");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(smpp::Smpp::from_url, "smpp", "smpps");
  reg!(smseagle::SmsEagle::from_url, "smseagle", "smseagles");
  reg!(smsmanager::SmsManager::from_url, "smsmanager", "smsmgr");
  reg!(smtp2go::Smtp2Go::from_url, "smtp2go");
  reg!(sns::Sns::from_url, "sns");
  reg!(sparkpost::SparkPost::from_url, "sparkpost");
  reg!(spike::Spike::from_url, "spike");
  reg!(splunk::Splunk::from_url, "splunk", "victorops");
  reg!(spugpush::SpugPush::from_url, "spugpush");
  reg!(streamlabs::Streamlabs::from_url, "strmlabs");
  reg!(synology::Synology::from_url, "synology", "synologys");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(syslog::Syslog::from_url, "syslog");
  reg!(techuluspush::TechulusPush::from_url, "techulus");
  reg!(telegram::Telegram::from_url, "tgram");
  reg!(threema::Threema::from_url, "threema");
  reg!(twilio::Twilio::from_url, "twilio");
  reg!(twist::Twist::from_url, "twist");
  reg!(twitter::Twitter::from_url, "twitter", "x", "tweet");
  reg!(viber::Viber::from_url, "viber");
  reg!(voipms::VoipMs::from_url, "voipms");
  reg!(vapid::Vapid::from_url, "vapid");
  reg!(vonage::Vonage::from_url, "vonage", "nexmo");
  reg!(webexteams::WebexTeams::from_url, "wxteams", "webex");
  reg!(wecombot::WeComBot::from_url, "wecombot");
  reg!(whatsapp::WhatsApp::from_url, "whatsapp");
  reg!(workflows::Workflows::from_url, "workflow", "workflows");
  reg!(wxpusher::WxPusher::from_url, "wxpusher");
  reg!(xbmc::Xbmc::from_url, "xbmc", "xbmcs", "kodi", "kodis");
  #[cfg(not(target_arch = "wasm32"))]
  reg!(xmpp::Xmpp::from_url, "xmpp", "xmpps");
  reg!(zulip::Zulip::from_url, "zulip");
  #[cfg(all(feature = "email", not(target_arch = "wasm32")))]
  reg!(email::Email::from_url, "mailto", "mailtos");

  m
}

/// Instantiate a notifier from a URL string
pub fn from_url(url: &str) -> Option<Box<dyn Notify>> {
  let parsed = ParsedUrl::parse(url)?;

  // Direct schema match
  if let Some(factory) = registry().get(&parsed.schema) {
    if let Some(notifier) = factory(&parsed) {
      return Some(notifier);
    }
  }

  // For https:// and http:// URLs, try to match by hostname to known services
  if parsed.schema == "https" || parsed.schema == "http" {
    if let Some(ref host) = parsed.host {
      let host_lower = host.to_lowercase();
      let reg = registry();

      // Host-based service matching
      let host_patterns: &[(&str, &str)] = &[
        ("ntfy.sh", "ntfy"),
        ("chat.googleapis.com", "gchat"),
        ("alert.victorops.com", "splunk"),
        ("api.spike.sh", "spike"),
        ("www.pushplus.plus", "pushplus"),
        ("pushplus.plus", "pushplus"),
        ("qmsg.zendee.cn", "qq"),
        ("push.spug.cc", "spugpush"),
        ("push.spug.dev", "spugpush"),
        ("qyapi.weixin.qq.com", "wecombot"),
        ("n.tkte.ch", "notifico"),
        ("api.46elks.com", "46elks"),
        ("maker.ifttt.com", "ifttt"),
        ("discord.com", "discord"),
        ("discordapp.com", "discord"),
        ("media.guilded.gg", "guilded"),
        ("hooks.slack.com", "slack"),
        ("hooks.slack-gov.com", "slack"),
        ("notica.us", "notica"),
        ("api.fluxer.app", "fluxer"),
        ("developer.lametric.com", "lametric"),
        ("api.ciscospark.com", "wxteams"),
        ("webexapis.com", "wxteams"),
        ("api.flock.com", "flock"),
        ("open.larksuite.com", "lark"),
        ("open.feishu.cn", "feishu"),
        ("webhooks.t2bot.io", "matrix"),
      ];

      for (pattern, schema_key) in host_patterns {
        if host_lower == *pattern || host_lower.ends_with(&format!(".{}", pattern)) {
          if let Some(factory) = reg.get(*schema_key) {
            if let Some(notifier) = factory(&parsed) {
              return Some(notifier);
            }
          }
        }
      }

      // MSTeams: outlook.office.com or *.webhook.office.com
      if host_lower == "outlook.office.com" || host_lower.ends_with(".webhook.office.com") {
        if let Some(factory) = reg.get("msteams") {
          if let Some(notifier) = factory(&parsed) {
            return Some(notifier);
          }
        }
      }

      // Workflows: *.azure.com with /workflows/ in path
      if host_lower.ends_with(".azure.com") && (parsed.path.contains("workflows/") || parsed.path.starts_with("workflows/")) {
        if let Some(factory) = reg.get("workflow") {
          if let Some(notifier) = factory(&parsed) {
            return Some(notifier);
          }
        }
      }

      // Apprise API: URLs with /notify/ in path
      if parsed.path.contains("notify/") || parsed.path.starts_with("notify/") {
        if let Some(factory) = reg.get("apprise") {
          if let Some(notifier) = factory(&parsed) {
            return Some(notifier);
          }
        }
      }

      // Ryver: *.ryver.com
      if host_lower.ends_with(".ryver.com") {
        if let Some(factory) = reg.get("ryver") {
          if let Some(notifier) = factory(&parsed) {
            return Some(notifier);
          }
        }
      }

      // Mattermost: URLs containing /hooks/ in the path
      if parsed.path.starts_with("hooks/") || parsed.path.contains("/hooks/") || parsed.path.contains("hooks") {
        if let Some(factory) = reg.get("mmost") {
          if let Some(notifier) = factory(&parsed) {
            return Some(notifier);
          }
        }
      }
    }
  }

  None
}
