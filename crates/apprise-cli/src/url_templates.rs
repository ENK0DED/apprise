//! URL templates for `apprise --details`, keyed by schema.
//!
//! Generated from Apprise 2.1.0 by
//! `scripts/gen_url_templates.py`; do not edit by hand.

/// The URL forms upstream documents for the service behind `schema`
/// (lowercase), as upstream's `details()` lists them.
#[rustfmt::skip]
pub fn url_templates(schema: &str) -> &'static [&'static str] {
  match schema {
    "46elks" | "elks" => &[
      "{schema}://{user}:{password}@{from_phone}",
      "{schema}://{user}:{password}@{from_phone}/{targets}",
    ],
    "amqp" | "amqps" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "apprise" | "apprises" => &[
      "{schema}://{host}/{token}",
      "{schema}://{host}:{port}/{token}",
      "{schema}://:{password}@{host}/{token}",
      "{schema}://:{password}@{host}:{port}/{token}",
      "{schema}://{user}@{host}/{token}",
      "{schema}://{user}@{host}:{port}/{token}",
      "{schema}://{user}:{password}@{host}/{token}",
      "{schema}://{user}:{password}@{host}:{port}/{token}",
    ],
    "aprs" => &[
      "{schema}://{user}:{password}@{targets}",
    ],
    "atalk" => &[
      "{schema}://{appuser}@{apikey}/{targets}",
    ],
    "azure" | "o365" => &[
      "{schema}://{source}/{tenant}/{client_id}/{secret}",
      "{schema}://{source}/{tenant}/{client_id}/{secret}/{targets}",
      "{schema}://{source}/{client_id}/{secret}",
      "{schema}://{source}/{client_id}/{secret}/{targets}",
    ],
    "bark" | "barks" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "betterstack" => &[
      "{schema}://{token}",
      "{schema}://{token}/{from_addr}",
      "{schema}://{token}/{targets}",
      "{schema}://{token}/{from_addr}/{targets}",
      "{schema}://{token}@{host}",
      "{schema}://{token}@{host}:{port}",
    ],
    "blink1" => &[
      "{schema}://",
      "{schema}://{serial}/",
    ],
    "bluesky" | "bsky" => &[
      "{schema}://{user}@{password}",
    ],
    "brevo" => &[
      "{schema}://{apikey}:{from_email}",
      "{schema}://{apikey}:{from_email}/{targets}",
    ],
    "bulksms" => &[
      "{schema}://{token_id}:{token_secret}@{targets}",
    ],
    "bulkvs" => &[
      "{schema}://{user}:{password}@{from_phone}/{targets}",
      "{schema}://{user}:{password}@{from_phone}",
    ],
    "burstsms" => &[
      "{schema}://{apikey}:{secret}@{sender_id}/{targets}",
    ],
    "chanify" => &[
      "{schema}://{token}",
    ],
    "clickatell" => &[
      "{schema}://{apikey}/{targets}",
      "{schema}://{source}@{apikey}/{targets}",
    ],
    "clicksend" => &[
      "{schema}://{user}:{apikey}@{targets}",
    ],
    "cliq" | "zcliq" | "zohocliq" => &[
      "{schema}://{token}/{targets}",
      "{schema}://{botname}@{token}/{targets}",
    ],
    "cm" => &[
      "{schema}://{token}/{targets}",
      "{schema}://{source}@{token}/{targets}",
    ],
    "d7sms" => &[
      "{schema}://{token}@{targets}",
    ],
    "dapnet" => &[
      "{schema}://{user}:{password}@{targets}",
    ],
    "dbus" | "kde" | "qt" => &[
      "{schema}://",
    ],
    "deltachat" | "deltachats" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{host}/{targets}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "dingtalk" => &[
      "{schema}://{token}/",
      "{schema}://{token}/{targets}/",
      "{schema}://{secret}@{token}/",
      "{schema}://{secret}@{token}/{targets}/",
    ],
    "discord" => &[
      "{schema}://{webhook_id}/{webhook_token}",
      "{schema}://{botname}@{webhook_id}/{webhook_token}",
    ],
    "dot" => &[
      "{schema}://{apikey}@{device_id}/",
    ],
    "eight00com" => &[
      "{schema}://{token}@{from_phone}",
      "{schema}://{token}@{from_phone}/{targets}",
    ],
    "emby" | "embys" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "enigma2" | "enigma2s" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
      "{schema}://{host}/{fullpath}",
      "{schema}://{host}:{port}/{fullpath}",
      "{schema}://{user}@{host}/{fullpath}",
      "{schema}://{user}@{host}:{port}/{fullpath}",
      "{schema}://{user}:{password}@{host}/{fullpath}",
      "{schema}://{user}:{password}@{host}:{port}/{fullpath}",
    ],
    "evolution" | "evolutions" => &[
      "{schema}://{apikey}@{host}/{instance}/{targets}",
      "{schema}://{apikey}@{host}:{port}/{instance}/{targets}",
    ],
    "exotel" => &[
      "{schema}://{sid}:{token}@{from_phone}",
      "{schema}://{sid}:{token}@{from_phone}/{targets}",
    ],
    "fcm" => &[
      "{schema}://{project}/{targets}?keyfile={keyfile}",
      "{schema}://{apikey}/{targets}",
    ],
    "feishu" => &[
      "{schema}://{token}",
    ],
    "flock" => &[
      "{schema}://{token}",
      "{schema}://{botname}@{token}",
      "{schema}://{botname}@{token}/{targets}",
      "{schema}://{token}/{targets}",
    ],
    "flowtriq" | "flowtriqs" => &[
      "{schema}://{apikey}@{host}/{path}/",
      "{schema}://{apikey}@{host}:{port}/{path}/",
    ],
    "fluxer" | "fluxers" => &[
      "{schema}://{webhook_id}/{webhook_token}",
      "{schema}://{host}/{webhook_id}/{webhook_token}",
      "{schema}://{host}:{port}/{webhook_id}/{webhook_token}",
      "{schema}://{host}{path}{webhook_id}/{webhook_token}",
      "{schema}://{host}:{port}{path}{webhook_id}/{webhook_token}",
      "{schema}://{botname}@{webhook_id}/{webhook_token}",
      "{schema}://{botname}@{host}:{port}/{webhook_id}/{webhook_token}",
      "{schema}://{botname}@{host}:{port}{path}{webhook_id}/{webhook_token}",
    ],
    "form" | "forms" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "freemobile" => &[
      "{schema}://{user}@{password}",
    ],
    "gchat" => &[
      "{schema}://{workspace}/{webhook_key}/{webhook_token}",
      "{schema}://{workspace}/{webhook_key}/{webhook_token}/{thread_key}",
    ],
    "gio" | "glib" => &[
      "{schema}://",
    ],
    "github" => &[
      "{schema}://{token}@{owner}/{repo}",
      "{schema}://{token}@{owner}/{repo}/{kind}",
      "{schema}://{token}@{owner}/{repo}/{kind}/{number}",
    ],
    "gnome" => &[
      "{schema}://",
    ],
    "goalert" | "goalerts" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{host}{path}{targets}",
      "{schema}://{host}:{port}{path}{targets}",
    ],
    "gorush" | "gorushs" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "gotify" | "gotifys" => &[
      "{schema}://{host}/{token}",
      "{schema}://{host}:{port}/{token}",
      "{schema}://{host}{path}{token}",
      "{schema}://{host}:{port}{path}{token}",
    ],
    "groundmail" => &[
      "{schema}://{apikey}:{from_addr}",
      "{schema}://{apikey}:{from_addr}/{targets}",
    ],
    "groupme" => &[
      "{schema}://{bot_id}",
      "{schema}://{bot_id}/{token}",
    ],
    "growl" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{password}@{host}",
      "{schema}://{password}@{host}:{port}",
    ],
    "guilded" => &[
      "{schema}://{webhook_id}/{webhook_token}",
      "{schema}://{botname}@{webhook_id}/{webhook_token}",
    ],
    "gupshup" => &[
      "{schema}://{app}:{apikey}@{source}/{targets}",
    ],
    "hassio" | "hassios" => &[
      "{schema}://{host}/{accesstoken}",
      "{schema}://{host}/{accesstoken}/{targets}",
      "{schema}://{host}:{port}/{accesstoken}",
      "{schema}://{host}:{port}/{accesstoken}/{targets}",
      "{schema}://{user}@{host}/{accesstoken}",
      "{schema}://{user}@{host}/{accesstoken}/{targets}",
      "{schema}://{user}@{host}:{port}/{accesstoken}",
      "{schema}://{user}@{host}:{port}/{accesstoken}/{targets}",
      "{schema}://{user}:{password}@{host}/{accesstoken}",
      "{schema}://{user}:{password}@{host}/{accesstoken}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{accesstoken}",
      "{schema}://{user}:{password}@{host}:{port}/{accesstoken}/{targets}",
    ],
    "httpsms" => &[
      "{schema}://{apikey}@{from_phone}",
      "{schema}://{apikey}@{from_phone}/{targets}",
    ],
    "humhub" | "humhubs" => &[
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "ifttt" => &[
      "{schema}://{webhook_id}/{events}",
    ],
    "ilert" => &[
      "{schema}://{key}",
    ],
    "incidentio" => &[
      "{schema}://{source}/{token}",
    ],
    "infobip" => &[
      "{schema}://{apikey}@{host}/{targets}",
    ],
    "irc" | "ircs" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "jellyfin" | "jellyfins" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "jira" => &[
      "{schema}://{apikey}",
      "{schema}://{user}@{apikey}",
      "{schema}://{apikey}/{targets}",
      "{schema}://{user}@{apikey}/{targets}",
    ],
    "join" => &[
      "{schema}://{apikey}/{targets}",
    ],
    "json" | "jsons" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "kaleyra" | "klr" => &[
      "{schema}://{sid}:{apikey}@{sender}/{targets}",
    ],
    "kavenegar" => &[
      "{schema}://{apikey}/{targets}",
      "{schema}://{source}@{apikey}/{targets}",
    ],
    "kodi" | "kodis" | "xbmc" | "xbmcs" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "kook" => &[
      "{schema}://{token}",
      "{schema}://{token}/{targets}",
    ],
    "kumulos" => &[
      "{schema}://{apikey}/{serverkey}",
    ],
    "lametric" | "lametrics" => &[
      "{schema}://{app_token}@{app_id}",
      "{schema}://{app_token}@{app_id}/{app_ver}",
      "{schema}://{apikey}@{host}",
      "{schema}://{user}:{apikey}@{host}",
      "{schema}://{apikey}@{host}:{port}",
      "{schema}://{user}:{apikey}@{host}:{port}",
    ],
    "lark" => &[
      "{schema}://{token}",
    ],
    "lauther" => &[
      "{schema}://{token}",
    ],
    "lfg" => &[
      "{schema}://{token}",
    ],
    "line" => &[
      "{schema}://{token}/{targets}",
    ],
    "macosx" => &[
      "{schema}://",
    ],
    "mailersend" => &[
      "{schema}://{apikey}:{from_email}",
      "{schema}://{apikey}:{from_email}/{targets}",
    ],
    "mailgun" => &[
      "{schema}://{user}@{host}/{apikey}/",
      "{schema}://{user}@{host}/{apikey}/{targets}",
    ],
    "mailto" | "mailtos" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{host}/{targets}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "mastodon" | "mastodons" | "toot" | "toots" => &[
      "{schema}://{token}@{host}",
      "{schema}://{token}@{host}:{port}",
      "{schema}://{token}@{host}/{targets}",
      "{schema}://{token}@{host}:{port}/{targets}",
    ],
    "matrix" | "matrixs" => &[
      "{schema}://{token}",
      "{schema}://{user}@{token}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
      "{schema}://{token}@{host}/{targets}",
      "{schema}://{token}@{host}:{port}/{targets}",
      "{schema}://{user}:{token}@{host}/{targets}",
      "{schema}://{user}:{token}@{host}:{port}/{targets}",
    ],
    "misskey" | "misskeys" => &[
      "{schema}://{token}@{host}",
      "{schema}://{token}@{host}:{port}",
    ],
    "mmost" | "mmosts" => &[
      "{schema}://{host}/{token}",
      "{schema}://{host}:{port}/{token}",
      "{schema}://{host}/{fullpath}/{token}",
      "{schema}://{host}:{port}/{fullpath}/{token}",
      "{schema}://{user}@{host}/{token}",
      "{schema}://{user}@{host}:{port}/{token}",
      "{schema}://{user}@{host}/{fullpath}/{token}",
      "{schema}://{user}@{host}:{port}/{fullpath}/{token}",
    ],
    "mobilemessage" | "mobilemsg" => &[
      "{schema}://{user}:{password}@{sender}/{targets}",
    ],
    "mqtt" | "mqtts" => &[
      "{schema}://{user}@{host}/{topic}",
      "{schema}://{user}@{host}:{port}/{topic}",
      "{schema}://{user}:{password}@{host}/{topic}",
      "{schema}://{user}:{password}@{host}:{port}/{topic}",
    ],
    "msg91" => &[
      "{schema}://{template}@{authkey}/{targets}",
    ],
    "msgbird" => &[
      "{schema}://{apikey}/{source}",
      "{schema}://{apikey}/{source}/{targets}",
    ],
    "ncloud" | "nclouds" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "nctalk" | "nctalks" => &[
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
      "{schema}://{host}/{targets}?secret={secret}",
      "{schema}://{host}:{port}/{targets}?secret={secret}",
    ],
    "nexmo" | "vonage" => &[
      "{schema}://{apikey}:{secret}@{from_phone}",
      "{schema}://{apikey}:{secret}@{from_phone}/{targets}",
    ],
    "notica" | "noticas" => &[
      "{schema}://{token}",
      "{schema}://{host}/{token}",
      "{schema}://{host}:{port}/{token}",
      "{schema}://{user}@{host}/{token}",
      "{schema}://{user}@{host}:{port}/{token}",
      "{schema}://{user}:{password}@{host}/{token}",
      "{schema}://{user}:{password}@{host}:{port}/{token}",
      "{schema}://{host}{path}/{token}",
      "{schema}://{host}:{port}/{path}/{token}",
      "{schema}://{user}@{host}/{path}/{token}",
      "{schema}://{user}@{host}:{port}{path}/{token}",
      "{schema}://{user}:{password}@{host}{path}/{token}",
      "{schema}://{user}:{password}@{host}:{port}/{path}/{token}",
    ],
    "notifiarr" => &[
      "{schema}://{apikey}/{targets}",
    ],
    "notifico" | "notificos" => &[
      "{schema}://{project_id}/{msghook}",
      "{schema}://{host}/{project_id}/{msghook}",
      "{schema}://{host}:{port}/{project_id}/{msghook}",
      "{schema}://{user}@{host}/{project_id}/{msghook}",
      "{schema}://{user}@{host}:{port}/{project_id}/{msghook}",
      "{schema}://{user}:{password}@{host}/{project_id}/{msghook}",
      "{schema}://{user}:{password}@{host}:{port}/{project_id}/{msghook}",
    ],
    "notifyre" => &[
      "{schema}://{apikey}/{targets}",
    ],
    "ntfy" | "ntfys" => &[
      "{schema}://{topic}",
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{user}@{host}/{targets}",
      "{schema}://{user}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
      "{schema}://{token}@{host}/{targets}",
      "{schema}://{token}@{host}:{port}/{targets}",
    ],
    "octopush" => &[
      "{schema}://{api_login}/{api_key}/{targets}",
      "{schema}://{sender}:{api_login}/{api_key}/{targets}",
    ],
    "onebot" | "onebots" => &[
      "{schema}://{host}/{targets}",
      "{schema}://{host}:{port}/{targets}",
      "{schema}://{token}@{host}/{targets}",
      "{schema}://{token}@{host}:{port}/{targets}",
    ],
    "onesignal" => &[
      "{schema}://{app}@{apikey}/{targets}",
      "{schema}://{template}:{app}@{apikey}/{targets}",
    ],
    "opsgenie" => &[
      "{schema}://{apikey}",
      "{schema}://{user}@{apikey}",
      "{schema}://{apikey}/{targets}",
      "{schema}://{user}@{apikey}/{targets}",
    ],
    "pagerduty" => &[
      "{schema}://{integrationkey}@{apikey}",
      "{schema}://{integrationkey}@{apikey}/{source}",
      "{schema}://{integrationkey}@{apikey}/{source}/{component}",
    ],
    "pagertree" => &[
      "{schema}://{integration}",
    ],
    "parsep" | "parseps" => &[
      "{schema}://{app_id}:{master_key}@{host}",
      "{schema}://{app_id}:{master_key}@{host}:{port}",
    ],
    "pbul" => &[
      "{schema}://{accesstoken}",
      "{schema}://{accesstoken}/{targets}",
    ],
    "pinglet" | "pinglets" => &[
      "{schema}://{token}@{host}/{namespace}/{topic}",
      "{schema}://{token}@{host}:{port}/{namespace}/{topic}",
      "{schema}://{token}@{host}{path}{namespace}/{topic}",
      "{schema}://{token}@{host}:{port}{path}{namespace}/{topic}",
    ],
    "pingram" => &[
      "{schema}://{apikey}/{targets}",
      "{schema}://{type}@{apikey}/{targets}",
    ],
    "pjet" | "pjets" => &[
      "{schema}://{host}:{port}/{secret_key}",
      "{schema}://{host}/{secret_key}",
      "{schema}://{user}:{password}@{host}:{port}/{secret_key}",
      "{schema}://{user}:{password}@{host}/{secret_key}",
    ],
    "plivo" => &[
      "{schema}://{auth_id}@{token}/{source}",
      "{schema}://{auth_id}@{token}/{source}/{targets}",
    ],
    "postmark" => &[
      "{schema}://{apikey}:{from_email}",
      "{schema}://{apikey}:{from_email}/{targets}",
    ],
    "pover" => &[
      "{schema}://{user_key}@{token}",
      "{schema}://{user_key}@{token}/{targets}",
    ],
    "prowl" => &[
      "{schema}://{apikey}",
      "{schema}://{apikey}/{providerkey}",
    ],
    "psafer" | "psafers" => &[
      "{schema}://{privatekey}",
      "{schema}://{privatekey}/{targets}",
    ],
    "push" => &[
      "{schema}://{apikey}",
    ],
    "pushdeer" | "pushdeers" => &[
      "{schema}://{pushkey}",
      "{schema}://{host}/{pushkey}",
      "{schema}://{host}:{port}/{pushkey}",
    ],
    "pushed" => &[
      "{schema}://{app_key}/{app_secret}",
      "{schema}://{app_key}/{app_secret}@{targets}",
    ],
    "pushme" => &[
      "{schema}://{token}",
    ],
    "pushplus" | "wecom" => &[
      "{schema}://{token}",
      "{schema}://{token}/{targets}",
    ],
    "pushward" => &[
      "{schema}://{apikey}",
    ],
    "pushy" => &[
      "{schema}://{apikey}/{targets}",
    ],
    "qq" => &[
      "{schema}://{token}",
    ],
    "reddit" => &[
      "{schema}://{user}:{password}@{app_id}/{app_secret}/{targets}",
    ],
    "resend" => &[
      "{schema}://{apikey}:{from_addr}",
      "{schema}://{apikey}:{from_addr}/{targets}",
    ],
    "revolt" => &[
      "{schema}://{bot_token}/{targets}",
    ],
    "ringc" => &[
      "{schema}://{from_phone}:{token}@{client_id}/{secret}/",
      "{schema}://{from_phone}:{token}@{client_id}/{secret}/{targets}",
      "{schema}://{token}@{client_id}/{secret}/{from_phone}",
      "{schema}://{token}@{client_id}/{secret}/{from_phone}/{targets}",
    ],
    "rocket" | "rockets" => &[
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{token}@{host}:{port}/{targets}",
      "{schema}://{user}:{token}@{host}/{targets}",
      "{schema}://{webhook}@{host}",
      "{schema}://{webhook}@{host}:{port}",
      "{schema}://{webhook}@{host}/{targets}",
      "{schema}://{webhook}@{host}:{port}/{targets}",
    ],
    "rsyslog" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{host}/{facility}",
      "{schema}://{host}:{port}/{facility}",
    ],
    "ryver" => &[
      "{schema}://{organization}/{token}",
      "{schema}://{botname}@{organization}/{token}",
    ],
    "schan" => &[
      "{schema}://{token}",
    ],
    "sendgrid" => &[
      "{schema}://{apikey}:{from_email}",
      "{schema}://{apikey}:{from_email}/{targets}",
    ],
    "sendpulse" => &[
      "{schema}://{user}@{host}/{client_secret}/",
      "{schema}://{user}@{host}/{client_id}/{client_secret}/{targets}",
    ],
    "serwersms" => &[
      "{schema}://{user}:{password}@{sender}/{targets}",
      "{schema}://{user}:{password}@{sender}",
    ],
    "ses" => &[
      "{schema}://{from_email}/{access_key_id}/{secret_access_key}/{region}/{targets}",
      "{schema}://{from_email}/{access_key_id}/{secret_access_key}/{region}",
    ],
    "session" | "sessions" | "sogs" => &[
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "seven" => &[
      "{schema}://{apikey}/{targets}",
    ],
    "sfr" => &[
      "{schema}://{user}:{password}@{space_id}/{targets}",
    ],
    "signal" | "signals" => &[
      "{schema}://{host}/{from_phone}",
      "{schema}://{host}:{port}/{from_phone}",
      "{schema}://{user}@{host}/{from_phone}",
      "{schema}://{user}@{host}:{port}/{from_phone}",
      "{schema}://{user}:{password}@{host}/{from_phone}",
      "{schema}://{user}:{password}@{host}:{port}/{from_phone}",
      "{schema}://{host}/{from_phone}/{targets}",
      "{schema}://{host}:{port}/{from_phone}/{targets}",
      "{schema}://{user}@{host}/{from_phone}/{targets}",
      "{schema}://{user}@{host}:{port}/{from_phone}/{targets}",
      "{schema}://{user}:{password}@{host}/{from_phone}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{from_phone}/{targets}",
    ],
    "signalgrid" => &[
      "{schema}://{client_key}/{targets}",
    ],
    "signl4" => &[
      "{schema}://{secret}",
    ],
    "simplepush" | "spush" => &[
      "{schema}://{token}",
      "{schema}://{token}/{targets}",
      "{schema}://{password}@{token}",
      "{schema}://{password}@{token}/{targets}",
    ],
    "sinch" => &[
      "{schema}://{service_plan_id}:{api_token}@{from_phone}",
      "{schema}://{service_plan_id}:{api_token}@{from_phone}/{targets}",
    ],
    "slack" => &[
      "{schema}://{token_a}/{token_b}/{token_c}",
      "{schema}://{botname}@{token_a}/{token_b}/{token_c}",
      "{schema}://{token_a}/{token_b}/{token_c}/{targets}",
      "{schema}://{botname}@{token_a}/{token_b}/{token_c}/{targets}",
      "{schema}://{access_token}/",
      "{schema}://{access_token}/{targets}",
    ],
    "smpp" | "smpps" => &[
      "{schema}://{user}:{password}@{host}/{from_phone}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{from_phone}/{targets}",
    ],
    "smsc" => &[
      "{schema}://{user}:{password}@{targets}",
    ],
    "smseagle" | "smseagles" => &[
      "{schema}://{token}@{host}/{targets}",
      "{schema}://{token}@{host}:{port}/{targets}",
    ],
    "smsmanager" | "smsmgr" => &[
      "{schema}://{apikey}@{targets}",
    ],
    "smtp2go" => &[
      "{schema}://{user}@{host}/{apikey}/",
      "{schema}://{user}@{host}/{apikey}/{targets}",
    ],
    "sns" => &[
      "{schema}://{token}@{access_key_id}/{secret_access_key}/{region}/{targets}",
      "{schema}://{access_key_id}/{secret_access_key}/{region}/{targets}",
    ],
    "sparkpost" => &[
      "{schema}://{user}@{host}/{apikey}/",
      "{schema}://{user}@{host}/{apikey}/{targets}",
    ],
    "spike" => &[
      "{schema}://{token}",
    ],
    "splunk" | "victorops" => &[
      "{schema}://{routing_key}@{apikey}",
      "{schema}://{routing_key}@{apikey}/{entity_id}",
    ],
    "spugpush" => &[
      "{schema}://{token}",
    ],
    "squadcast" => &[
      "{schema}://{key}",
    ],
    "stackfield" => &[
      "{schema}://{token}",
    ],
    "strmlabs" => &[
      "{schema}://{access_token}/",
    ],
    "synology" | "synologys" => &[
      "{schema}://{host}/{token}",
      "{schema}://{host}:{port}/{token}",
      "{schema}://{user}@{host}/{token}",
      "{schema}://{user}@{host}:{port}/{token}",
      "{schema}://{user}:{password}@{host}/{token}",
      "{schema}://{user}:{password}@{host}:{port}/{token}",
    ],
    "syslog" => &[
      "{schema}://",
      "{schema}://{facility}",
    ],
    "telesign" => &[
      "{schema}://{customer_id}:{apikey}@{targets}",
    ],
    "telnyx" => &[
      "{schema}://{apikey}@{from_phone}",
      "{schema}://{apikey}@{from_phone}/{targets}",
    ],
    "tgram" => &[
      "{schema}://{bot_token}",
      "{schema}://{bot_token}/{targets}",
    ],
    "threema" => &[
      "{schema}://{gateway_id}@{secret}/{targets}",
    ],
    "trigv" | "trigvs" => &[
      "{schema}://{api_key}",
      "{schema}://{api_key}/{targets}",
      "{schema}://{api_key}@{host}/{targets}",
      "{schema}://{api_key}@{host}:{port}/{targets}",
      "{schema}://{api_key}@{host}",
    ],
    "tweet" | "twitter" | "x" => &[
      "{schema}://{ckey}/{csecret}/{akey}/{asecret}",
      "{schema}://{ckey}/{csecret}/{akey}/{asecret}/{targets}",
    ],
    "twilio" => &[
      "{schema}://{account_sid}:{auth_token}@{from_phone}",
      "{schema}://{account_sid}:{auth_token}@{from_phone}/{targets}",
    ],
    "twist" => &[
      "{schema}://{password}:{email}",
      "{schema}://{password}:{email}/{targets}",
    ],
    "vapid" => &[
      "{schema}://{subscriber}",
      "{schema}://{subscriber}/{targets}",
    ],
    "viber" => &[
      "{schema}://{token}/{targets}",
    ],
    "voipms" => &[
      "{schema}://{password}:{email}/{from_phone}/{targets}",
    ],
    "webex" | "wxteams" => &[
      "{schema}://{token}",
      "{schema}://{access_token}/{targets}",
    ],
    "wechat" => &[
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}/{targets}",
    ],
    "wecombot" => &[
      "{schema}://{key}",
    ],
    "whatsapp" => &[
      "{schema}://{token}@{from_phone_id}/{targets}",
      "{schema}://{template}:{token}@{from_phone_id}/{targets}",
    ],
    "windows" => &[
      "{schema}://",
    ],
    "workflow" | "workflows" => &[
      "{schema}://{host}/{workflow}/{signature}",
      "{schema}://{host}:{port}/{workflow}/{signature}",
    ],
    "wpush" => &[
      "{schema}://{apikey}",
      "{schema}://{apikey}/{targets}",
    ],
    "wxpusher" => &[
      "{schema}://{token}/{targets}",
    ],
    "xml" | "xmls" => &[
      "{schema}://{host}",
      "{schema}://{host}:{port}",
      "{schema}://{user}@{host}",
      "{schema}://{user}@{host}:{port}",
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
    ],
    "xmpp" | "xmpps" => &[
      "{schema}://{user}:{password}@{host}",
      "{schema}://{user}:{password}@{host}:{port}",
      "{schema}://{user}:{password}@{host}/{targets}",
      "{schema}://{user}:{password}@{host}:{port}/{targets}",
    ],
    "youlmk" => &[
      "{schema}://{token}",
    ],
    "zalo" => &[
      "{schema}://{token}/{targets}",
    ],
    "zoom" => &[
      "{schema}://{webhook_id}/{token}",
    ],
    "zulip" => &[
      "{schema}://{botname}@{organization}/{token}",
      "{schema}://{botname}@{organization}/{token}/{targets}",
    ],
    _ => &[],
  }
}
