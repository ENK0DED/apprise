![Apprise Logo](https://raw.githubusercontent.com/caronc/apprise/master/apprise/assets/themes/default/apprise-logo.png)

<hr/>

**ap·prise** / *verb*<br/>
To inform or tell (someone). To make one aware of something.
<hr/>

*Apprise* allows you to send a notification to *almost* all of the most popular *notification* services available to us today such as: Telegram, Discord, Slack, Amazon SNS, Gotify, etc.

This repository is a **Rust port** of [Apprise](https://github.com/caronc/apprise), the Python notification library and command line tool created and maintained by [Chris Caron](https://github.com/caronc). It follows upstream Apprise **2.1.0**: the same service URLs, the same TEXT and YAML configuration files, and an `apprise` command line tool with the same options and exit codes. It builds to a single binary that needs no Python, and it ships the library as a Rust crate (`apprise-core`) and a Node.js package (`@enk0ded/apprise`).

The credit for Apprise's design, its URL syntax and its knowledge of every service below belongs to Chris Caron and upstream's [contributors](ACKNOWLEDGEMENTS.md). Please report problems with this port in [this repository](https://github.com/ENK0DED/apprise/issues), not upstream.

* One notification library to rule them all.
* A common and intuitive notification syntax.
* Supports the handling of images and attachments (_to the notification services that will accept them_).
* It's incredibly lightweight.
* Amazing response times because all messages sent asynchronously.

Developers who wish to provide a notification service no longer need to research each and every one out there. They no longer need to try to adapt to the new ones that come out thereafter. They just need to include this one library and then they can immediately gain access to almost all of the notifications services available to us today.

System Administrators and DevOps who wish to send a notification now no longer need to find the right tool for the job. Everything is already wrapped and supported within the `apprise` command line tool (CLI) that ships with this product.

[![CI](https://github.com/ENK0DED/apprise/actions/workflows/CI.yml/badge.svg)](https://github.com/ENK0DED/apprise/actions/workflows/CI.yml)
[![License](https://img.shields.io/badge/license-BSD--2--Clause-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](https://www.rust-lang.org/)
[![Upstream](https://img.shields.io/badge/upstream-Apprise%202.1.0-informational.svg)](https://github.com/caronc/apprise)

Visit upstream's [Official Documentation](https://appriseit.com/getting-started/) site for more information on Apprise. Its service pages describe the URLs this port accepts as well.

# Table of Contents
<!--ts-->
* [Supported Notifications](#supported-notifications)
  * [Productivity Based Notifications](#productivity-based-notifications)
  * [SMS Notifications](#sms-notifications)
  * [Desktop Notifications](#desktop-notifications)
  * [Email Notifications](#email-notifications)
  * [Custom Notifications](#custom-notifications)
* [Installation](#installation)
  * [Command Line Tool (Cargo)](#command-line-tool-cargo)
  * [Docker](#docker)
  * [Node.js Package](#nodejs-package)
  * [Cargo Features](#cargo-features)
* [Command Line Usage](#command-line-usage)
  * [Configuration Files](#cli-configuration-files)
  * [Template Variables](#cli-template-variables)
  * [Tagging Support](#cli-tagging-support)
  * [File Attachments](#cli-file-attachments)
  * [Environment Variables](#cli-environment-variables)
  * [Exit Status](#cli-exit-status)
* [Developer API Usage](#developer-api-usage)
  * [Configuration Files](#api-configuration-files)
  * [Template Variables](#api-template-variables)
  * [File Attachments](#api-file-attachments)
  * [Node.js API](#nodejs-api)
* [Persistent Storage](#persistent-storage)
* [Differences from Upstream](#differences-from-upstream)
* [More Supported Links and Documentation](#want-to-learn-more)
<!--te-->

# Supported Notifications

The section identifies all of the services supported by this library. [Check out the upstream documentation for more information on the supported modules here](https://appriseit.com/services/).

A few services depend on [Cargo features](#cargo-features). The default build includes MQTT (`mqtt`), Email and Delta Chat (`email`), and Matrix and Simplepush end-to-end encryption (`matrix-e2ee`, `simplepush-encryption`); AMQP 0-9-1 is always built in. PGP/MIME email (`pgp`), AMQP 1.0 (`amqp`), the [desktop notifications](#desktop-notifications) (`desktop`) and Blink(1) (`blink1`) are off by default.

## Productivity Based Notifications

The table below identifies the services this tool supports and some example service URLs you need to use in order to take advantage of it. Click on any of the services listed below to get more details on how you can configure Apprise to access them. If you're having trouble constructing your own URL; try our [Apprise URL Builder](https://appriseit.com/tools/url-builder/) out.

| Notification Service | Service ID | Default Port | Example Syntax |
| -------------------- | ---------- | ------------ | -------------- |
| [AMQP](https://appriseit.com/services/amqp/) | amqp:// or amqps:// | (TCP) 5672 or 5671 | amqp://hostname/queue<br />amqp://user:pass@hostname/queue1/queue2<br />amqps://user:pass@hostname/?exchange=alerts<br />amqps://user:pass@hostname/?version=1.0&to=/queues/name
| [Apprise API](https://appriseit.com/services/apprise_api/)  | apprise:// or apprises:// | (TCP) 80 or 443 | apprise://hostname/Token<br/>apprise://:password@hostname/Token<br/>apprise://user:password@hostname/Token
| [AWS SES](https://appriseit.com/services/ses/)  | ses://   | (TCP) 443   | ses://user@domain/AccessKeyID/AccessSecretKey/RegionName<br/>ses://user@domain/AccessKeyID/AccessSecretKey/RegionName/email1/email2/emailN
| [Bark](https://appriseit.com/services/bark/)  | bark:// or barks://   | (TCP) 80 or 443   | bark://hostname<br />bark://hostname/device_key<br />bark://hostname/device_key1/device_key2/device_keyN<br/>barks://hostname<br />barks://hostname/device_key<br />barks://hostname/device_key1/device_key2/device_keyN
| [Better Stack](https://appriseit.com/services/betterstack/) | betterstack:// | (TCP) 443 | betterstack://token/email<br />betterstack://token/email/team<br />betterstack://token/email/team1/team2/teamN<br />betterstack://source_token@ingesting_host
| [Blink(1)](https://appriseit.com/services/blink1/) | blink1:// | USB | blink1://<br />blink1://serial/
| [BlueSky](https://appriseit.com/services/bluesky/) | bluesky:// or bsky://  | (TCP) 443   | bluesky://Handle:AppPw<br />bluesky://Handle:AppPw/TargetHandle<br />bluesky://Handle:AppPw/TargetHandle1/TargetHandle2/TargetHandleN
| [Brevo](https://appriseit.com/services/brevo/) | brevo://  | (TCP) 443   | brevo://APIToken:FromEmail/<br />brevo://APIToken:FromEmail/ToEmail<br />brevo://APIToken:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [Chanify](https://appriseit.com/services/chanify/) | chanify://    | (TCP) 443    | chanify://token
| [Delta Chat](https://appriseit.com/services/deltachat/) | deltachat:// or deltachats:// | (TCP) 25 or 587 | deltachat://user:pass@smtp.example.com/friend@example.org<br />deltachats://user:pass@smtp.example.com/friend1@example.org/friend2@example.org
| [Discord](https://appriseit.com/services/discord/)  | discord://   | (TCP) 443   | discord://webhook_id/webhook_token<br />discord://avatar@webhook_id/webhook_token
| [Dot.](https://appriseit.com/services/dot/)  | dot:// | (TCP) 443 | dot://apikey@device_id/text/<br />dot://apikey@device_id/image/<br />**Note**: `device_id` is the Quote/0 hardware serial
| [Emby](https://appriseit.com/services/emby/)  | emby:// or embys:// | (TCP) 8096 | emby://user@hostname/<br />emby://user:password@hostname
| [Enigma2](https://appriseit.com/services/enigma2/)  | enigma2:// or enigma2s:// | (TCP) 80 or 443 | enigma2://hostname
| [Evolution API](https://appriseit.com/services/evolution/) | evolution:// or evolutions:// | (TCP) 80 or 443 | evolution://apikey@hostname/instance/ToPhoneNo<br/>evolution://apikey@hostname:port/instance/ToPhoneNo<br/>evolution://apikey@hostname/instance/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [FCM](https://appriseit.com/services/fcm/) | fcm://    | (TCP) 443    | fcm://project@apikey/DEVICE_ID<br />fcm://project@apikey/#TOPIC<br/>fcm://project@apikey/DEVICE_ID1/#topic1/#topic2/DEVICE_ID2/
| [Feishu](https://appriseit.com/services/feishu/) | feishu://    | (TCP) 443    | feishu://token
| [Flock](https://appriseit.com/services/flock/) | flock://    | (TCP) 443    | flock://token<br/>flock://botname@token<br/>flock://app_token/u:userid<br/>flock://app_token/g:channel_id<br/>flock://app_token/u:userid/g:channel_id
| [Flowtriq](https://appriseit.com/services/flowtriq/) | flowtriq:// or flowtriqs://    | (TCP) 80 or 443    | flowtriq://apikey@hostname/webhook/path<br />flowtriqs://apikey@hostname/webhook/path
| [Fluxer](https://appriseit.com/services/fluxer/) | fluxer:// or fluxers:// | (TCP) 443 | fluxer://WebhookID/WebhookToken<br />fluxer://botname@WebhookID/WebhookToken<br />fluxers://hostname/WebhookID/WebhookToken<br />fluxers://hostname:port/path/WebhookID/WebhookToken
| [GitHub](https://appriseit.com/services/github/) | github://    | (TCP) 443    | github://token@owner/repo<br />github://token@owner/repo/issues/42<br />github://token@owner/repo/pull/1807<br />github://token@owner/repo/discussions/7
| [GoAlert](https://appriseit.com/services/goalert/) | goalert:// or goalerts://    | (TCP) 80 or 443    | goalert://hostname/integration_key<br />goalerts://hostname/integration_key1/integration_key2<br />goalerts://hostname/path/integration_key
| [Google Chat](https://appriseit.com/services/googlechat/) | gchat://    | (TCP) 443    | gchat://workspace/key/token
| [Gorush](https://appriseit.com/services/gorush/) | gorush:// or gorushs://   | (TCP) 8088, 80 or 443    | gorush://hostname:8088/device_token<br />gorush://hostname:8088/device_token1/device_token2?platform=ios<br />gorushs://user:password@hostname/device_token
| [Gotify](https://appriseit.com/services/gotify/) | gotify:// or gotifys://   | (TCP) 80 or 443    | gotify://hostname/token<br />gotifys://hostname/token?priority=high
| [Groundmail](https://appriseit.com/services/groundmail/) | groundmail://  | (TCP) 443   | groundmail://APIKey:FromEmail/<br />groundmail://APIKey:FromEmail/ToEmail<br />groundmail://APIKey:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [GroupMe](https://appriseit.com/services/groupme/) | groupme://   | (TCP) 443   | groupme://bot_id<br />groupme://bot_id/access_token
| [Growl](https://appriseit.com/services/growl/)  | growl://   | (UDP) 23053   | growl://hostname<br />growl://hostname:portno<br />growl://password@hostname<br />growl://password@hostname:port</br>**Note**: you can also use the get parameter _version_ which can allow the growl request to behave using the older v1.x protocol. An example would look like: growl://hostname?version=1
| [Gupshup](https://appriseit.com/services/gupshup/) | gupshup://   | (TCP) 443   | gupshup://AppName:ApiKey@FromPhoneNo/ToPhoneNo<br/>gupshup://AppName:ApiKey@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>gupshup://AppName:ApiKey@FromPhoneNo/ToPhoneNo?template=TemplateID&:body=1
| [Guilded](https://appriseit.com/services/guilded/)  | guilded://   | (TCP) 443   | guilded://webhook_id/webhook_token<br />guilded://avatar@webhook_id/webhook_token
| [Home Assistant](https://appriseit.com/services/homeassistant/)       | hassio:// or hassios://   | (TCP) 8123 or 443 | hassio://hostname/accesstoken<br />hassio://user@hostname/accesstoken<br />hassio://user:password@hostname:port/accesstoken<br />hassio://hostname/optional/path/accesstoken
| [HumHub](https://appriseit.com/services/humhub/) | humhub:// or humhubs:// | (TCP) 80 or 443 | humhubs://token@hostname/container_id<br />humhubs://user:password@hostname/container_id<br />humhubs://token@hostname/id1/id2/id3
| [IFTTT](https://appriseit.com/services/ifttt/) | ifttt://    | (TCP) 443    | ifttt://webhooksID/Event<br />ifttt://webhooksID/Event1/Event2/EventN<br/>ifttt://webhooksID/Event1/?+Key=Value<br/>ifttt://webhooksID/Event1/?-Key=value1
| [ilert](https://appriseit.com/services/ilert/) | ilert://  | (TCP) 443 | ilert://IntegrationKey<br />ilert://IntegrationKey?priority=high<br />ilert://IntegrationKey?action=resolve&dedup=key
| [incident.io](https://appriseit.com/services/incidentio/) | incidentio://  | (TCP) 443 | incidentio://source_id/token<br />incidentio://source_id/token?action=resolve&dedup=key
| [IRC](https://appriseit.com/services/irc/) | irc:// or ircs://   | (TCP) 6667 or 6697 | ircs://user:pass@irc.server/@user<br /> ircs://user:pass@irc.server/#channel?join=true&mode=nickserv<br/>ircs://user:pass@znc.server/@user1/@user2/@user3/#channel1
| [Jellyfin](https://appriseit.com/services/jellyfin/)  | jellyfin:// or jellyfins:// | (TCP) 8096 | jellyfin://user@hostname/<br />jellyfins://user:password@hostname
| [Jira](https://appriseit.com/services/jira/) | jira:// | (TCP) 443 | jira://APIKey<br/>jira://APIKey/@UserID<br/>jira://APIKey/#Team<br/>jira://APIKey/\*Schedule<br/>jira://APIKey/^Escalation
| [Join](https://appriseit.com/services/join/) | join://   | (TCP) 443    | join://apikey/device<br />join://apikey/device1/device2/deviceN/<br />join://apikey/group<br />join://apikey/groupA/groupB/groupN<br />join://apikey/DeviceA/groupA/groupN/DeviceN/
| [KODI](https://appriseit.com/services/kodi/) | kodi:// or kodis://<br/>xbmc:// or xbmcs://    | (TCP) 8080 or 443   | kodi://hostname<br />kodi://user@hostname<br />kodi://user:password@hostname:port
| [Kook](https://appriseit.com/services/kook/) | kook:// | (TCP) 443 | kook://token/channel_id<br/>kook://token/channel1/channel2<br/>kook://token/@user_id<br/>kook://webhook_key?mode=webhook
| [Kumulos](https://appriseit.com/services/kumulos/) | kumulos:// | (TCP) 443 | kumulos://apikey/serverkey
| [LaMetric Time](https://appriseit.com/services/lametric/) | lametric:// or lametrics:// | (TCP) 443 | lametric://apikey@device_ipaddr<br/>lametric://apikey@hostname:port<br/>lametric://client_id@client_secret
| [Lark](https://appriseit.com/services/lark/) | lark://  | (TCP) 443   | lark://BotToken
| [Lauther](https://appriseit.com/services/lauther/) | lauther:// | (TCP) 443 | lauther://Token
| [LFG Alarm](https://appriseit.com/services/lfgalarm/) | lfg:// | (TCP) 443 | lfg://Token
| [Line](https://appriseit.com/services/line/) | line:// | (TCP) 443 | line://Token@User<br/>line://Token/User1/User2/UserN
| [Mailgun](https://appriseit.com/services/mailgun/) | mailgun:// | (TCP) 443 | mailgun://user@hostname/apikey<br />mailgun://user@hostname/apikey/email<br />mailgun://user@hostname/apikey/email1/email2/emailN<br />mailgun://user@hostname/apikey/?name="From%20User"
| [MailerSend](https://appriseit.com/services/mailersend/) | mailersend://  | (TCP) 443   | mailersend://APIToken:FromEmail/<br />mailersend://APIToken:FromEmail/ToEmail<br />mailersend://APIToken:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [Mastodon](https://appriseit.com/services/mastodon/) | mastodon:// or mastodons://<br/>toot:// or toots://| (TCP) 80 or 443  | mastodon://access_key@hostname<br />mastodon://access_key@hostname/@user<br />mastodon://access_key@hostname/@user1/@user2/@userN
| [Matrix](https://appriseit.com/services/matrix/) | matrix:// or matrixs://  | (TCP) 80 or 443 | matrix://hostname<br />matrix://user@hostname<br />matrixs://user:pass@hostname:port/#room_alias<br />matrixs://user:pass@hostname:port/!room_id<br />matrixs://user:pass@hostname:port/#room_alias/!room_id/#room2<br />matrixs://token@hostname:port/?webhook=matrix<br />matrix://user:token@hostname/?webhook=slack&format=markdown
| [Mattermost](https://appriseit.com/services/mattermost/) | mmost:// or mmosts:// | (TCP) 8065 | mmost://hostname/authkey<br />mmost://hostname:80/authkey<br />mmost://user@hostname:80/authkey<br />mmost://hostname/authkey?channel=channel<br />mmosts://hostname/authkey<br />mmosts://user@hostname/authkey<br />
| [Microsoft Power Automate / Workflows (MSTeams)](https://appriseit.com/services/workflows/) | workflows:// or workflow://  | (TCP) 443   | workflows://WorkflowID/Signature/
| [Misskey](https://appriseit.com/services/misskey/) | misskey:// or misskeys://| (TCP) 80 or 443  | misskey://access_token@hostname
| [MQTT](https://appriseit.com/services/mqtt/) | mqtt://  or mqtts:// | (TCP) 1883 or 8883   | mqtt://hostname/topic<br />mqtt://user@hostname/topic<br />mqtts://user:pass@hostname:9883/topic
| [Nextcloud](https://appriseit.com/services/nextcloud/) | ncloud:// or nclouds:// | (TCP) 80 or 443 | ncloud://adminuser:pass@host/User<br/>nclouds://adminuser:pass@host/User1/User2/UserN
| [NextcloudTalk](https://appriseit.com/services/nextcloudtalk/) | nctalk:// or nctalks:// | (TCP) 80 or 443 | nctalk://user:pass@host/RoomId<br/>nctalks://user:pass@host/RoomId1/RoomId2/RoomIdN<br/>nctalks://host/RoomId?secret=BotSecret
| [Notica](https://appriseit.com/services/notica/) | notica:// or noticas://  | (TCP) 443   | notica://Token/
| [Notifiarr](https://appriseit.com/services/notifiarr/) | notifiarr:// | (TCP) 443 | notifiarr://apikey/#channel<br />notifiarr://apikey/#channel1/#channel2/#channeln
| [Notifico](https://appriseit.com/services/notifico/) | notifico:// or notificos://  | (TCP) 80 or 443   | notifico://ProjectID/MessageHook/<br />notifico://host/ProjectID/MessageHook/<br />notificos://host/ProjectID/MessageHook/
| [ntfy](https://appriseit.com/services/ntfy/) | ntfy:// or ntfys://  | (TCP) 80 or 443   | ntfy://topic/<br/>ntfys://topic/
| [Octopush](https://appriseit.com/services/octopush/) | octopush:// | (TCP) 443 | octopush://APILogin/APIKey/TargetPhoneNo<br />octopush://Sender:APILogin/APIKey/TargetPhoneNo<br />octopush://Sender:APILogin/APIKey/TargetPhoneNo1/TargetPhoneNo2/TargetPhoneNoN
| [Office 365](https://appriseit.com/services/office365/) | o365:// or azure://  | (TCP) 443   | o365://TenantID:AccountEmail/ClientID/ClientSecret<br />o365://TenantID:AccountEmail/ClientID/ClientSecret/TargetEmail<br />o365://TenantID:AccountEmail/ClientID/ClientSecret/TargetEmail1/TargetEmail2/TargetEmailN
| [OneBot (QQ)](https://appriseit.com/services/onebot/) | onebot:// or onebots:// | (TCP) 80 or 443 | onebot://hostname:port/UserID<br/>onebot://token@hostname:port/@UserID/#GroupID<br/>onebots://token@hostname:port/@UserID1/@UserID2/#GroupID1/#GroupIDN
| [OneSignal](https://appriseit.com/services/onesignal/) | onesignal:// | (TCP) 443 | onesignal://AppID@APIKey/PlayerID<br/>onesignal://TemplateID:AppID@APIKey/UserID<br/>onesignal://AppID@APIKey/#IncludeSegment<br/>onesignal://AppID@APIKey/Email
| [Opsgenie](https://appriseit.com/services/opsgenie/) | opsgenie:// | (TCP) 443 | opsgenie://APIKey<br/>opsgenie://APIKey/UserID<br/>opsgenie://APIKey/#Team<br/>opsgenie://APIKey/\*Schedule<br/>opsgenie://APIKey/^Escalation
| [PagerDuty](https://appriseit.com/services/pagerduty/) | pagerduty:// | (TCP) 443 | pagerduty://IntegrationKey@ApiKey<br/>pagerduty://IntegrationKey@ApiKey/Source/Component
| [PagerTree](https://appriseit.com/services/pagertree/) | pagertree:// | (TCP) 443 | pagertree://integration_id
| [ParsePlatform](https://appriseit.com/services/parseplatform/) | parsep:// or parseps:// | (TCP) 80 or 443 | parsep://AppID:MasterKey@Hostname<br/>parseps://AppID:MasterKey@Hostname
| [Pingram](https://appriseit.com/services/pingram/) | pingram://  | (TCP) 443   | pingram://ApiKey/Target<br />pingram://ApiKey/Target1/Target2/TargetN<br />pingram://MessageType@ApiKey/Target
| [Pinglet](https://appriseit.com/services/pinglet/) | pinglet:// or pinglets:// | (TCP) 80 or 443 | pinglets://apikey@hostname/namespace/topic<br/>pinglets://apikey@hostname:port/namespace/topic
| [Postmark](https://appriseit.com/services/postmark/) | postmark://  | (TCP) 443   | postmark://APIToken:FromEmail/<br />postmark://APIToken:FromEmail/ToEmail<br />postmark://APIToken:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [Prowl](https://appriseit.com/services/prowl/) | prowl://   | (TCP) 443    | prowl://apikey<br />prowl://apikey/providerkey
| [PushBullet](https://appriseit.com/services/pushbullet/) | pbul://    | (TCP) 443    | pbul://accesstoken<br />pbul://accesstoken/#channel<br/>pbul://accesstoken/A_DEVICE_ID<br />pbul://accesstoken/email@address.com<br />pbul://accesstoken/#channel/#channel2/email@address.net/DEVICE
| [Pushjet](https://appriseit.com/services/pushjet/) | pjet:// or pjets:// | (TCP) 80 or 443 | pjet://hostname/secret<br />pjet://hostname:port/secret<br />pjets://secret@hostname/secret<br />pjets://hostname:port/secret
| [Push (Techulus)](https://appriseit.com/services/techulus/) | push://    | (TCP) 443    | push://apikey/
| [Pushed](https://appriseit.com/services/pushed/) | pushed://    | (TCP) 443    | pushed://appkey/appsecret/<br/>pushed://appkey/appsecret/#ChannelAlias<br/>pushed://appkey/appsecret/#ChannelAlias1/#ChannelAlias2/#ChannelAliasN<br/>pushed://appkey/appsecret/@UserPushedID<br/>pushed://appkey/appsecret/@UserPushedID1/@UserPushedID2/@UserPushedIDN
| [PushMe](https://appriseit.com/services/pushme/) | pushme://  | (TCP) 443   | pushme://Token/
| [Pushover](https://appriseit.com/services/pushover/)  | pover://   | (TCP) 443   | pover://user@token<br />pover://user@token/DEVICE<br />pover://user@token/DEVICE1/DEVICE2/DEVICEN<br />**Note**: you must specify both your user_id and token
| [Pushplus](https://appriseit.com/services/pushplus/) | pushplus:// or wecom://  | (TCP) 443   | pushplus://Token<br />wecom://Token (WeCom channel)
| [PushSafer](https://appriseit.com/services/pushsafer/)  | psafer:// or psafers://  | (TCP) 80 or 443  | psafer://privatekey<br />psafers://privatekey/DEVICE<br />psafer://privatekey/DEVICE1/DEVICE2/DEVICEN
| [PushWard](https://appriseit.com/services/pushward/)  | pushward://  | (TCP) 443  | pushward://hlk_apikey<br />pushward://hlk_apikey?level=critical&volume=0.8
| [Pushy](https://appriseit.com/services/pushy/)  | pushy://  | (TCP) 443  | pushy://apikey/DEVICE<br />pushy://apikey/DEVICE1/DEVICE2/DEVICEN<br />pushy://apikey/TOPIC<br />pushy://apikey/TOPIC1/TOPIC2/TOPICN
| [PushDeer](https://appriseit.com/services/pushdeer/) | pushdeer:// or pushdeers:// | (TCP) 80 or 443 | pushdeer://pushKey<br />pushdeer://hostname/pushKey<br />pushdeer://hostname:port/pushKey
| [QQ Push](https://appriseit.com/services/qq/) | qq://  | (TCP) 443   | qq://Token
| [Reddit](https://appriseit.com/services/reddit/) | reddit:// | (TCP) 443   | reddit://user:password@app_id/app_secret/subreddit<br />reddit://user:password@app_id/app_secret/sub1/sub2/subN
| [Resend](https://appriseit.com/services/resend/) | resend://  | (TCP) 443   | resend://APIToken:FromEmail/<br />resend://APIToken:FromEmail/ToEmail<br />resend://APIToken:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [Revolt](https://appriseit.com/services/revolt/) | revolt:// | (TCP) 443   |  revolt://bottoken/ChannelID<br />revolt://bottoken/ChannelID1/ChannelID2/ChannelIDN |
| [Rocket.Chat](https://appriseit.com/services/rocketchat/) | rocket:// or rockets://  | (TCP) 80 or 443   | rocket://user:password@hostname/RoomID/Channel<br />rockets://user:password@hostname:443/#Channel1/#Channel1/RoomID<br />rocket://user:password@hostname/#Channel<br />rocket://webhook@hostname<br />rockets://webhook@hostname/@User/#Channel
| [RSyslog](https://appriseit.com/services/rsyslog/) | rsyslog://  | (UDP) 514 | rsyslog://hostname<br />rsyslog://hostname/Facility
| [Ryver](https://appriseit.com/services/ryver/) | ryver://  | (TCP) 443   | ryver://Organization/Token<br />ryver://botname@Organization/Token
| [SendGrid](https://appriseit.com/services/sendgrid/) | sendgrid://  | (TCP) 443   | sendgrid://APIToken:FromEmail/<br />sendgrid://APIToken:FromEmail/ToEmail<br />sendgrid://APIToken:FromEmail/ToEmail1/ToEmail2/ToEmailN/
| [SendPulse](https://appriseit.com/services/sendpulse/) | sendpulse://  | (TCP) 443   | sendpulse://user@host/ClientId/ClientSecret<br />sendpulse://user@host/ClientId/clientSecret/ToEmail<br />sendpulse://user@host/ClientId/ClientSecret/ToEmail1/ToEmail2/ToEmailN/
| [ServerChan](https://appriseit.com/services/serverchan/) | schan://   | (TCP) 443    | schan://sendkey/
| [Session Open Group Server](https://appriseit.com/services/sogs/) | session://, sessions:// or sogs:// | (TCP) 80 or 443 | sessions://public_key:seed@hostname/room<br />sessions://public_key:seed@hostname/room1/room2<br />sessions://public_key:seed@hostname:port/room<br />session://public_key:seed@hostname/room
| [Signal API](https://appriseit.com/services/signal/) | signal://  or signals:// | (TCP) 80 or 443  | signal://hostname:port/FromPhoneNo<br/>signal://hostname:port/FromPhoneNo/ToPhoneNo<br/>signal://hostname:port/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Signalgrid](https://appriseit.com/services/signalgrid/) | signalgrid://  | (TCP) 443   | signalgrid://CLIENT_KEY/CHANNEL<br />signalgrid://CLIENT_KEY/CHANNEL1/CHANNEL2<br />signalgrid://CLIENT_KEY/CHANNEL?critical=true
| [SIGNL4](https://appriseit.com/services/signl4/) | signl4://  | (TCP) 80 or 443  | signl4://hostname
| [Simplepush](https://appriseit.com/services/simplepush/) | spush:// or simplepush://   | (TCP) 443    | spush://APIToken<br />spush://APIToken/Topic<br />spush://Password@APIToken/Topic<br />spush://IntegrationToken/@Member<br />spush://IntegrationToken/?broadcast=yes
| [Slack](https://appriseit.com/services/slack/) | slack://  | (TCP) 443   | slack://TokenA/TokenB/TokenC/<br />slack://TokenA/TokenB/TokenC/Channel<br />slack://botname@TokenA/TokenB/TokenC/Channel<br />slack://user@TokenA/TokenB/TokenC/Channel1/Channel2/ChannelN
| [SMTP2Go](https://appriseit.com/services/smtp2go/) | smtp2go:// | (TCP) 443 | smtp2go://user@hostname/apikey<br />smtp2go://user@hostname/apikey/email<br />smtp2go://user@hostname/apikey/email1/email2/emailN<br />smtp2go://user@hostname/apikey/?name="From%20User"
| [SparkPost](https://appriseit.com/services/sparkpost/) | sparkpost:// | (TCP) 443 | sparkpost://user@hostname/apikey<br />sparkpost://user@hostname/apikey/email<br />sparkpost://user@hostname/apikey/email1/email2/emailN<br />sparkpost://user@hostname/apikey/?name="From%20User"
| [Spike.sh](https://appriseit.com/services/spike/) | spike://  | (TCP) 443   | spike://Token
| [Splunk](https://appriseit.com/services/splunk/) | splunk:// or victorops:// | (TCP) 443 | splunk://route_key@apikey<br />splunk://route_key@apikey/entity_id
| [Spug Push](https://appriseit.com/services/spugpush/) | spugpush://  | (TCP) 443   | spugpush://Token
| [Squadcast](https://appriseit.com/services/squadcast/) | squadcast://  | (TCP) 443   | squadcast://WebhookKey<br />squadcast://WebhookKey?region=eu<br />squadcast://WebhookKey?action=resolve&dedup=key
| [Stackfield](https://appriseit.com/services/stackfield/) | stackfield://  | (TCP) 443   | stackfield://WebhookToken
| [Streamlabs](https://appriseit.com/services/streamlabs/) | strmlabs:// | (TCP) 443 | strmlabs://AccessToken/<br/>strmlabs://AccessToken/?name=name&identifier=identifier&amount=0&currency=USD
| [Synology Chat](https://appriseit.com/services/synology_chat/) | synology:// or synologys:// |  (TCP) 80 or 443 | synology://hostname/token<br />synology://hostname:port/token
| [Syslog](https://appriseit.com/services/syslog/) | syslog://  | n/a | syslog://<br />syslog://Facility
| [Telegram](https://appriseit.com/services/telegram/) | tgram://  | (TCP) 443   | tgram://bottoken/ChatID<br />tgram://bottoken/ChatID1/ChatID2/ChatIDN
| [Twitter](https://appriseit.com/services/twitter/) | twitter://, x:// or tweet://  | (TCP) 443   | twitter://CKey/CSecret/AKey/ASecret<br/>twitter://user@CKey/CSecret/AKey/ASecret<br/>twitter://CKey/CSecret/AKey/ASecret/User1/User2/User2<br/>twitter://CKey/CSecret/AKey/ASecret?mode=tweet
| [Twist](https://appriseit.com/services/twist/) | twist://  | (TCP) 443   | twist://password:login<br/>twist://password:login/#channel<br/>twist://password:login/#team:channel<br/>twist://password:login/#team:channel1/channel2/#team3:channel
| [Vapid (WebPush)](https://appriseit.com/services/vapid/) | vapid://    | (TCP) 443    | vapid://subscriber/target<br/>vapid://subscriber/target?subfile=path&keyfile=path
| [Viber](https://appriseit.com/services/viber/) | viber://    | (TCP) 443    | viber://token/target
| [Webex Teams (Cisco)](https://appriseit.com/services/wxteams/) | wxteams:// or webex://  | (TCP) 443   | wxteams://Token
| [WeChat (WeCom)](https://appriseit.com/services/wechat/) | wechat://  | (TCP) 443   | wechat://CorpID:AppSecret@AgentID/@all<br/>wechat://CorpID:AppSecret@AgentID/UserID
| [WeCom Bot](https://appriseit.com/services/wecombot/) | wecombot://  | (TCP) 443   | wecombot://BotKey
| [WhatsApp](https://appriseit.com/services/whatsapp/) | whatsapp://  | (TCP) 443   | whatsapp://AccessToken@FromPhoneID/ToPhoneNo<br/>whatsapp://Template:AccessToken@FromPhoneID/ToPhoneNo
| [WPUSH](https://appriseit.com/services/wpush/) | wpush://  | (TCP) 443   | wpush://APIKey<br/>wpush://APIKey/Topic1/Topic2/TopicN
| [WxPusher](https://appriseit.com/services/wxpusher/) | wxpusher://  | (TCP) 443   | wxpusher://AppToken@UserID1/UserID2/UserIDN<br/>wxpusher://AppToken@Topic1/Topic2/Topic3<br/>wxpusher://AppToken@UserID1/Topic1/
| [XMPP](https://appriseit.com/services/xmpp/) | xmpp:// or xmpps://    | (TCP) 5222 or 5223   | xmpp://user:pass@hostname<br />xmpps://user:pass@hostname/jid<br />xmpps://user:pass@hostname/jid1/jid2@example.ca
| [YouLMK](https://appriseit.com/services/youlmk/) | youlmk://  | (TCP) 443   | youlmk://ylk_token<br/>youlmk://k_key<br/>youlmk://ylk_token?priority=critical<br/>youlmk://ylk_token?failure=critical&url=https://example.com/runs/1
| [Zalo](https://appriseit.com/services/zalo/) | zalo://  | (TCP) 443   | zalo://BotToken/ChatID<br/>zalo://BotToken/ChatID1/ChatID2/ChatIDN
| [Zoho Cliq](https://appriseit.com/services/cliq/) | cliq:// or zcliq:// or zohocliq://  | (TCP) 443   | cliq://Token/Channel<br />cliq://Token/@Bot<br />cliq://BotName@Token/Channel1/Channel2/@Bot?region=eu
| [Zoom](https://appriseit.com/services/zoom/) | zoom://  | (TCP) 443   | zoom://WebhookID/Token
| [Zulip Chat](https://appriseit.com/services/zulip/) | zulip://  | (TCP) 443   | zulip://botname@Organization/Token<br />zulip://botname@Organization/Token/Stream<br />zulip://botname@Organization/Token/Email

## SMS Notifications

SMS Notifications for the most part do not have a both a `title` and `body`.  They consist of a single `body` which is usually no more then 160 characters in length.  When using Apprise, the `title` and `body` are therefore combined into a single message prior to their transmission.

| Notification Service | Service ID | Default Port | Example Syntax |
| -------------------- | ---------- | ------------ | -------------- |
| [46elks](https://appriseit.com/services/46elks/) | 46elks:// or elks://  | (TCP) 443   | 46elks://user:password@FromPhoneNo<br/>46elks://user:password@FromPhoneNo/ToPhoneNo<br/>46elks://user:password@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [800.com](https://appriseit.com/services/eight00com/) | eight00com://  | (TCP) 443   | eight00com://Token@FromPhoneNo<br/>eight00com://Token@FromPhoneNo/ToPhoneNo<br/>eight00com://Token@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Africas Talking](https://appriseit.com/services/africas_talking/) | atalk://  | (TCP) 443   | atalk://AppUser@ApiKey/ToPhoneNo<br/>atalk://AppUser@ApiKey/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Automated Packet Reporting System (APRS)](https://appriseit.com/services/aprs/)  | aprs:// | (TCP) 10152 | aprs://user:pass@callsign<br/>aprs://user:pass@callsign1/callsign2/callsignN
| [AWS SNS](https://appriseit.com/services/sns/)  | sns://   | (TCP) 443   | sns://AccessKeyID/AccessSecretKey/RegionName/+PhoneNo<br/>sns://AccessKeyID/AccessSecretKey/RegionName/+PhoneNo1/+PhoneNo2/+PhoneNoN<br/>sns://AccessKeyID/AccessSecretKey/RegionName/Topic<br/>sns://AccessKeyID/AccessSecretKey/RegionName/Topic1/Topic2/TopicN
| [BulkSMS](https://appriseit.com/services/bulksms/) | bulksms://  | (TCP) 443   | bulksms://user:password@ToPhoneNo<br/>bulksms://User:Password@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [BulkVS](https://appriseit.com/services/bulkvs/) | bulkvs://  | (TCP) 443   | bulkvs://user:password@FromPhoneNo<br/>bulkvs://user:password@FromPhoneNo/ToPhoneNo<br/>bulkvs://user:password@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Burst SMS](https://appriseit.com/services/burstsms/) | burstsms://  | (TCP) 443   | burstsms://ApiKey:ApiSecret@FromPhoneNo/ToPhoneNo<br/>burstsms://ApiKey:ApiSecret@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Clickatell](https://appriseit.com/services/clickatell/)                         | clickatell://               | (TCP) 443       | clickatell://ApiKey/ToPhoneNo<br/>clickatell://FromPhoneNo@ApiKey/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [ClickSend](https://appriseit.com/services/clicksend/) | clicksend://  | (TCP) 443   | clicksend://user:pass@PhoneNo<br/>clicksend://user:pass@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [CM.com](https://appriseit.com/services/cm/) | cm://  | (TCP) 443   | cm://ProductToken/ToPhoneNo<br/>cm://SenderID@ProductToken/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [DAPNET](https://appriseit.com/services/dapnet/) | dapnet://  | (TCP) 80   | dapnet://user:pass@callsign<br/>dapnet://user:pass@callsign1/callsign2/callsignN
| [D7 Networks](https://appriseit.com/services/d7networks/) | d7sms://  | (TCP) 443   | d7sms://token@PhoneNo<br/>d7sms://token@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [DingTalk](https://appriseit.com/services/dingtalk/)  | dingtalk://   | (TCP) 443   | dingtalk://token/<br />dingtalk://token/ToPhoneNo<br />dingtalk://token/ToPhoneNo1/ToPhoneNo2/ToPhoneNo1/
| [Exotel](https://appriseit.com/services/exotel/) | exotel://  | (TCP) 443   | exotel://sid:token@FromPhoneNo<br/>exotel://sid:token@FromPhoneNo/ToPhoneNo<br/>exotel://sid:token@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [Free-Mobile](https://appriseit.com/services/freemobile/)  | freemobile://   | (TCP) 443   | freemobile://user@password/
| [httpSMS](https://appriseit.com/services/httpsms/) | httpsms://  | (TCP) 443   | httpsms://ApiKey@FromPhoneNo<br/>httpsms://ApiKey@FromPhoneNo/ToPhoneNo<br/>httpsms://ApiKey@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Infobip](https://appriseit.com/services/infobip/) | infobip://  | (TCP) 443   | infobip://ApiKey@BaseURL/ToPhoneNo<br/>infobip://ApiKey@BaseURL/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/?from=SenderID
| [Kaleyra](https://appriseit.com/services/kaleyra/) | kaleyra:// or klr://  | (TCP) 443   | kaleyra://SID:ApiKey@SenderID/ToPhoneNo<br/>kaleyra://SID:ApiKey@SenderID/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>kaleyra://SID:ApiKey@SenderID/ToPhoneNo?region=in&template=TemplateID
| [Kavenegar](https://appriseit.com/services/kavenegar/) | kavenegar://  | (TCP) 443   | kavenegar://ApiKey/ToPhoneNo<br/>kavenegar://FromPhoneNo@ApiKey/ToPhoneNo<br/>kavenegar://ApiKey/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [MessageBird](https://appriseit.com/services/messagebird/) | msgbird://  | (TCP) 443   | msgbird://ApiKey/FromPhoneNo<br/>msgbird://ApiKey/FromPhoneNo/ToPhoneNo<br/>msgbird://ApiKey/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Mobile Message](https://appriseit.com/services/mobilemessage/) | mobilemessage:// or mobilemsg://  | (TCP) 443   | mobilemessage://ApiUser:ApiPass@SenderID/ToPhoneNo<br/>mobilemessage://ApiUser:ApiPass@SenderID/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [MSG91](https://appriseit.com/services/msg91/) | msg91://  | (TCP) 443   | msg91://TemplateID@AuthKey/ToPhoneNo<br/>msg91://TemplateID@AuthKey/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Plivo](https://appriseit.com/services/plivo/) | plivo://  | (TCP) 443   | plivo://AuthID@Token@FromPhoneNo<br/>plivo://AuthID@Token/FromPhoneNo/ToPhoneNo<br/>plivo://AuthID@Token/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [RingCentral](https://appriseit.com/services/ringcentral/) | ringc://  | (TCP) 443   | ringc://SourcePhoneNo:Password@ClientID/ClientSecret<br/>ringc://SourcePhoneNo:JWTToken@ClientID/ClientSecret/ToPhoneNo<br/>ringc://SourcePhoneNo:JWTToken@ClientID/ClientSecret/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [SerwerSMS](https://appriseit.com/services/serwersms/) | serwersms://  | (TCP) 443   | serwersms://user:password@SenderName/+ToPhoneNo<br/>serwersms://user:password@SenderName/+ToPhoneNo1/+ToPhoneNo2<br/>serwersms://user:password@SenderName/%23GroupID
| [Seven](https://appriseit.com/services/seven/)                                   | seven://                    | (TCP) 443   | seven://ApiKey/FromPhoneNo<br/>seven://ApiKey/FromPhoneNo/ToPhoneNo<br/>seven://ApiKey/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Société Française du Radiotéléphone (SFR)](https://appriseit.com/services/sfr/) | sfr://   | (TCP) 443    | sfr://user:password@spaceId/ToPhoneNo<br/>sfr://user:password@spaceId/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Signal API](https://appriseit.com/services/signal/) | signal://  or signals:// | (TCP) 80 or 443  | signal://hostname:port/FromPhoneNo<br/>signal://hostname:port/FromPhoneNo/ToPhoneNo<br/>signal://hostname:port/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Sinch](https://appriseit.com/services/sinch/) | sinch://  | (TCP) 443   | sinch://ServicePlanId:ApiToken@FromPhoneNo<br/>sinch://ServicePlanId:ApiToken@FromPhoneNo/ToPhoneNo<br/>sinch://ServicePlanId:ApiToken@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>sinch://ServicePlanId:ApiToken@ShortCode/ToPhoneNo<br/>sinch://ServicePlanId:ApiToken@ShortCode/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [SMPP](https://appriseit.com/services/smpp/)    | smpp:// or smpps://  | (TCP) 443  | smpp://user:password@hostname:port/FromPhoneNo/ToPhoneNo<br/>smpps://user:password@hostname:port/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN
| [SMSEagle](https://appriseit.com/services/smseagle/) | smseagle:// or smseagles:// | (TCP) 80 or 443  | smseagles://hostname:port/ToPhoneNo<br/>smseagles://hostname:port/@ToContact<br/>smseagles://hostname:port/#ToGroup<br/>smseagles://hostname:port/ToPhoneNo1/#ToGroup/@ToContact/
| [SMS Manager](https://appriseit.com/services/sms_manager/) | smsmgr:// or smsmanager://  | (TCP) 443   | smsmgr://ApiKey@ToPhoneNo<br/>smsmgr://ApiKey@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [SMSC](https://appriseit.com/services/smsc/) | smsc://  | (TCP) 443   | smsc://login:password@ToPhoneNo<br/>smsc://login:password@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Telesign](https://appriseit.com/services/telesign/) | telesign://  | (TCP) 443   | telesign://CustomerID:ApiKey@ToPhoneNo<br/>telesign://CustomerID:ApiKey@ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Telnyx](https://appriseit.com/services/telnyx/) | telnyx://  | (TCP) 443   | telnyx://ApiKey@FromPhoneNo<br/>telnyx://ApiKey@FromPhoneNo/ToPhoneNo<br/>telnyx://ApiKey@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>telnyx://ApiKey@FromPhoneNo/ToPhoneNo?profile=MessagingProfileID
| [Threema Gateway](https://appriseit.com/services/threema/) | threema://  | (TCP) 443   | threema://GatewayID@secret/ToPhoneNo<br/>threema://GatewayID@secret/ToEmail<br/>threema://GatewayID@secret/ToThreemaID/<br/>threema://GatewayID@secret/ToEmail/ToThreemaID/ToPhoneNo/...
| [Trigv](https://trigv.com/) | trigv:// or trigvs:// | (TCP) 80 or 443 | trigvs://APIKey<br/>trigvs://APIKey/channel<br/>trigv://APIKey@hostname/channel
| [Notifyre](https://appriseit.com/services/notifyre/) | notifyre://  | (TCP) 443   | notifyre://ApiKey/ToPhoneNo<br/>notifyre://ApiKey/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN<br/>notifyre://ApiKey/ToFaxNo?mode=fax<br/>notifyre://ApiKey/ToFaxNo?mode=fax&from=+FromNo
| [Twilio](https://appriseit.com/services/twilio/) | twilio://  | (TCP) 443   | twilio://AccountSid:AuthToken@FromPhoneNo<br/>twilio://AccountSid:AuthToken@FromPhoneNo/ToPhoneNo<br/>twilio://AccountSid:AuthToken@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>twilio://AccountSid:AuthToken@FromPhoneNo/ToPhoneNo?apikey=Key<br/>twilio://AccountSid:AuthToken@ShortCode/ToPhoneNo<br/>twilio://AccountSid:AuthToken@ShortCode/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/<br/>twilio://AccountSid:AuthToken@FromPhoneNo/ToPhoneNo?method=call<br/>twilio://AccountSid:AuthToken@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN?method=call
| [Voipms](https://appriseit.com/services/voipms/) | voipms://  | (TCP) 443   | voipms://password:email/FromPhoneNo<br/>voipms://password:email/FromPhoneNo/ToPhoneNo<br/>voipms://password:email/FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/
| [Vonage](https://appriseit.com/services/vonage/) (formerly Nexmo) | vonage:// or nexmo://  | (TCP) 443   | vonage://ApiKey:ApiSecret@FromPhoneNo<br/>vonage://ApiKey:ApiSecret@FromPhoneNo/ToPhoneNo<br/>vonage://ApiKey:ApiSecret@FromPhoneNo/ToPhoneNo1/ToPhoneNo2/ToPhoneNoN/

## Desktop Notifications

These need the `desktop` feature, which is off by default. `dbus://`, `qt://`, `kde://`, `glib://`, `gio://` and `gnome://` work on Linux and other Unix systems with a D-Bus session bus, `macosx://` on macOS through [terminal-notifier](https://github.com/julienXX/terminal-notifier), and `windows://` on Windows.

| Notification Service | Service ID | Default Port | Example Syntax |
| -------------------- | ---------- | ------------ | -------------- |
| [Linux DBus Notifications](https://appriseit.com/services/dbus/)  | dbus://<br />qt://<br />kde://<br />glib://<br />gio://  | n/a  | dbus://<br />qt://<br />kde://<br />glib://<br />gio://
| [Linux Gnome Notifications](https://appriseit.com/services/gnome/) | gnome://    |        n/a          | gnome://
| [MacOS X Notifications](https://appriseit.com/services/macosx/) | macosx://    |        n/a          | macosx://
| [Windows Notifications](https://appriseit.com/services/windows/) | windows://    |        n/a          | windows://

## Email Notifications

Email (and Delta Chat) needs the `email` feature, which is on by default. PGP/MIME signing and encryption (`?pgp=sign`, `?pgp=encrypt`, `?wkd=yes`) also needs the `pgp` feature, which is off by default; without it such a URL loads with a warning and its notifications fail, as upstream Apprise does without PGPy.

| Service ID | Default Port | Example Syntax |
| ---------- | ------------ | -------------- |
| [mailto://](https://appriseit.com/services/email/)  |  (TCP) 25    | mailto://userid:pass@domain.com<br />mailto://domain.com?user=userid&pass=password<br/>mailto://domain.com:2525?user=userid&pass=password<br />mailto://user@gmail.com&pass=password<br />mailto://mySendingUsername:mySendingPassword@example.com?to=receivingAddress@example.com<br />mailto://userid:password@example.com?smtp=mail.example.com&from=noreply@example.com&name=no%20reply
| [mailtos://](https://appriseit.com/services/email/) |  (TCP) 587   | mailtos://userid:pass@domain.com<br />mailtos://domain.com?user=userid&pass=password<br/>mailtos://domain.com:465?user=userid&pass=password<br />mailtos://user@hotmail.com&pass=password<br />mailtos://mySendingUsername:mySendingPassword@example.com?to=receivingAddress@example.com<br />mailtos://userid:password@example.com?smtp=mail.example.com&from=noreply@example.com&name=no%20reply

Apprise has some email services built right into it (such as yahoo, fastmail, hotmail, gmail, etc) that greatly simplify the mailto:// service.  See more details [here](https://appriseit.com/services/email/).

## Custom Notifications

| Post Method          | Service ID | Default Port | Example Syntax |
| -------------------- | ---------- | ------------ | -------------- |
| [Form](https://appriseit.com/services/form/)       | form:// or forms://   | (TCP) 80 or 443 | form://hostname<br />form://user@hostname<br />form://user:password@hostname:port<br />form://hostname/a/path/to/post/to
| [JSON](https://appriseit.com/services/json/)       | json:// or jsons://   | (TCP) 80 or 443 | json://hostname<br />json://user@hostname<br />json://user:password@hostname:port<br />json://hostname/a/path/to/post/to
| [XML](https://appriseit.com/services/xml/)         | xml:// or xmls://   | (TCP) 80 or 443 | xml://hostname<br />xml://user@hostname<br />xml://user:password@hostname:port<br />xml://hostname/a/path/to/post/to

# Installation

CI publishes only the Node.js package. There are no prebuilt `apprise` binaries or Docker images yet, so build the command line tool with Cargo or Docker.

## Command Line Tool (Cargo)

You need Rust 1.96 or newer (install it with [rustup](https://rustup.rs/)). From a clone of this repository:
```bash
git clone https://github.com/ENK0DED/apprise.git
cd apprise

# Build the apprise binary and install it into ~/.cargo/bin
cargo install --path crates/apprise-cli

# The same with optional features added (see Cargo Features below)
cargo install --path crates/apprise-cli --features pgp,desktop
```

To build without installing, run `cargo build --release -p apprise-cli`; the binary is `target/release/apprise`. The release profile optimises for size, and TLS comes from rustls, so the binary needs no OpenSSL or Python.

## Docker

The repository's `Dockerfile` builds the CLI on Alpine with the default features plus `pgp` and `amqp`, and copies it into a small Alpine image whose entry point is `apprise`. The image includes Alpine's `tzdata`, so named time zones (`?tz=Europe/Paris`) work in it:
```bash
docker build -t apprise .
docker run --rm apprise -vv -t 'my title' -b 'my notification body' \
   'pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b'

# Mount a configuration file and point --config at it
docker run --rm -v "$PWD/apprise.yml:/config/apprise.yml:ro" apprise \
   --config=/config/apprise.yml --tag=devops -b 'Deployment finished'
```

The image sets `APPRISE_STORAGE_PATH=/data/cache` for [persistent storage](#persistent-storage). `docker-compose.yml` mounts a named volume on `/data` and passes `APPRISE_URLS` and `APPRISE_CONFIG_PATH` through from your shell:
```bash
export APPRISE_URLS='pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b'
docker compose run --rm apprise -b 'Hello from Docker'
```

## Node.js Package

`@enk0ded/apprise` is the library as a native Node.js addon (built with [napi-rs](https://napi.rs/)). CI builds it for macOS (x64, arm64), Windows (x64, arm64), Linux with glibc or musl (x64, arm64) and Android (arm64), and publishes it to npm when a release commit lands on `master`. It needs Node.js 25 or newer, and also runs under Bun.
```bash
npm install @enk0ded/apprise
```

To build it from this repository instead (you need Rust and [Bun](https://bun.sh/)):
```bash
cd crates/apprise-napi
bun install
bun run build        # release build; use build:debug for a quicker debug build
bun test
```

The package is built with every feature the binding has, which includes PGP/MIME email and AMQP 1.0; the desktop and Blink(1) services are not included. See [Node.js API](#nodejs-api) for how to use it.

## Cargo Features

`apprise-core` has the features below, and the CLI forwards each of them under the same name. The size column is how much each one adds to the x86-64 Linux release binary of the CLI, which is about 8.8 MB with the default features, 10.3 MB with `pgp` added and 7.5 MB with no features at all. Sizes vary by platform.

| Feature | Default | What it enables | Release size |
| ------- | ------- | --------------- | ------------ |
| `email` | on | `mailto://`, `mailtos://` (SMTP) and `deltachat://` | about 98 KB |
| `yaml` | on | YAML configuration files (TEXT configuration always works) | about 574 KB |
| `mqtt` | on | `mqtt://` and `mqtts://` (a built-in client; no extra crates) | about 29 KB |
| `images` | on | Bundles the default theme's PNG icons, which Telegram (`?image=yes`) uploads and desktop notifications show | about 176 KB (only the icon sizes in use are kept; all of them are 286 KB) |
| `simplepush-encryption` | on | Simplepush end-to-end encryption (`spush://password@...`); without it, encrypted Simplepush URLs cannot send | about 57 KB |
| `matrix-e2ee` | on | Matrix end-to-end encryption (Olm/Megolm, SAS device verification, encrypted attachments); without it, nothing is sent to encrypted rooms | about 280 KB |
| `pgp` | off | PGP/MIME signing and encryption for email and Delta Chat through rPGP (`?pgp=`, `pgppub=`, `pgpprv=`, `?wkd=yes`); turns on `email`. Without it a PGP mode loads with a warning and its notifications fail | about 1.5 MB |
| `amqp` | off | AMQP 1.0 (`?version=1.0`) through fe2o3-amqp. `amqp://` and `amqps://` over AMQP 0-9-1 are always built in; without the feature a `?version=1.0` URL does not load | about 540 KB |
| `desktop` | off | The [desktop notifications](#desktop-notifications) for the platform you build on | about 587 KB |
| `blink1` | off | `blink1://` USB LED notifications through hidapi; without it, `blink1://` URLs load but cannot send | about 58 KB |

A program that uses the library picks its features in its own `Cargo.toml`:
```toml
[dependencies]
apprise-core = { git = "https://github.com/ENK0DED/apprise", default-features = false, features = ["email", "yaml"] }
```

To build a smaller CLI, turn the defaults off and pick what you need, for example `cargo install --path crates/apprise-cli --no-default-features --features email,yaml`.

# Command Line Usage

A command line tool named *apprise* is included. Provide one or more service URLs to send a notification:

```bash
# Send to several services. The -vv option shows additional details.
apprise -vv -t 'my title' -b 'my notification body' \
   'mailto://myemail:mypass@gmail.com' \
   'pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b'

# If you don't specify a --body (-b) then stdin is used allowing
# you to use the tool as part of your every day administration:
cat /proc/cpuinfo | apprise -vv -t 'cpu info' \
   'mailto://myemail:mypass@gmail.com'

# The title field is totally optional
uptime | apprise -vv \
   'discord:///4174216298/JHMHI8qBe7bk2ZwO5U711o3dV_js'

# See what would be notified without sending anything
apprise --dry-run 'mailto://myemail:mypass@gmail.com'

# List every service this build supports
apprise --details
```

Run `apprise --help` for every option; the [man page](packaging/man/apprise.md) describes each of them.

## CLI Configuration Files

No one wants to put their credentials out for everyone to see on the command line. No problem, *apprise* also supports configuration files. It can handle both a specific YAML format or a very simple TEXT format. You can also pull these configuration files via an HTTP query too! Read more about the expected structure of the configuration files [here](https://appriseit.com/getting-started/configuration/).

```bash
# By default if no url or configuration is specified apprise will attempt to load
# configuration files (if present) from:
#  ~/.apprise
#  ~/.apprise.conf
#  ~/.apprise.yml
#  ~/.apprise.yaml
#  ~/.config/apprise
#  ~/.config/apprise.conf
#  ~/.config/apprise.yml
#  ~/.config/apprise.yaml

# Also subdirectory handling:
#  ~/.apprise/apprise
#  ~/.apprise/apprise.conf
#  ~/.apprise/apprise.yml
#  ~/.apprise/apprise.yaml
#  ~/.config/apprise/apprise
#  ~/.config/apprise/apprise.conf
#  ~/.config/apprise/apprise.yml
#  ~/.config/apprise/apprise.yaml

# And system-wide:
#  /etc/apprise
#  /etc/apprise.yml
#  /etc/apprise.yaml
#  /etc/apprise/apprise
#  /etc/apprise/apprise.conf
#  /etc/apprise/apprise.yml
#  /etc/apprise/apprise.yaml

# Windows users can store their default configuration files here (each
# also as apprise, apprise.yml or apprise.yaml):
#  %APPDATA%/Apprise/apprise.conf
#  %LOCALAPPDATA%/Apprise/apprise.conf
#  %ALLUSERSPROFILE%\Apprise\apprise.conf
#  %PROGRAMFILES%\Apprise\apprise.conf
#  %COMMONPROGRAMFILES%\Apprise\apprise.conf

# If you loaded one of those files, your command line gets really easy:
apprise -vv -t 'my title' -b 'my notification body'

# If you want to deviate from the default paths or specify more than one,
# just specify them using the --config switch:
apprise -vv -t 'my title' -b 'my notification body' \
   --config=/path/to/my/config.yml

# Got lots of configuration locations? No problem, you can specify them all:
# Apprise can even fetch the configuration from over a network!
apprise -vv -t 'my title' -b 'my notification body' \
   --config=/path/to/my/config.yml \
   --config=https://localhost/my/apprise/config
```

A TEXT configuration lists one URL per line, optionally preceded by the tags that select it, and can define groups of tags:
```ini
# Untagged: notified when no --tag is given
mailto://myemail:mypass@gmail.com

# Tagged entries
devops=pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b
devops,critical=discord://4174216298/JHMHI8qBe7bk2ZwO5U711o3dV_js

# A group: --tag=oncall notifies everything tagged devops or admin
oncall=devops,admin
```

The same in YAML:
```yaml
version: 1

groups:
  oncall: devops, admin

urls:
  - mailto://myemail:mypass@gmail.com
  - pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b:
      - tag: devops
  - discord://4174216298/JHMHI8qBe7bk2ZwO5U711o3dV_js:
      - tag: devops, critical
```

A configuration can `include` another one; `--recursion-depth` (`-R`) sets how many levels are followed (1 by default, 0 ignores them).

## CLI Template Variables

A YAML configuration can leave a value out of a URL and have it filled in
later, which lets you keep the configuration in version control without the
secrets in it.

Write `${NAME}` where the value belongs and declare every name you use in a
`template:` section:

```yaml
template:
  # A default is used unless the call supplies a value
  smtp_host: smtp.example.com
  # No default: the call or environment must fill this name
  api_key:

urls:
  - sendgrid://${API_KEY}:noreply@example.com/you@example.com:
      - tag: alerts
```

Apprise looks for each value in this order and stops at the first answer:

1. a value passed with `--template-var` (`-tv`)
2. the default written in the `template:` section
3. the environment variable `APPRISE_TEMPLATE_<NAME>`

The environment is the last resort, so it only ever fills in a name the
`template:` section declared without a default.

Values supplied for a call or through the environment are trimmed. If one is
blank afterwards, it reads as not supplied and the next source above applies.
A default written in the configuration stays as written. Use `name: ""` for an
explicit empty-string default; `name:` means the value is required.

```bash
# Supply it directly
apprise --config=apprise.yml --tag=alerts \
   --template-var api_key=your-secret-key \
   --body="Hello"

# Or leave it in the environment
export APPRISE_TEMPLATE_API_KEY=your-secret-key
apprise --config=apprise.yml --tag=alerts --body="Hello"
```

A service missing a required value is skipped, with an error in the log, while
the other services are still notified.

Only names declared under `template:` are replaced. Names ignore case, values
keep their original case, and each value may be up to 1,024 characters.
Undeclared `${...}` text remains unchanged, preventing similar text in an
existing password from being replaced by accident. Extra supplied names are
accepted and ignored.

Variables may be placed directly in a URL or in a named YAML setting. URL
markers can change any field, including the destination; whole-host markers
also accept `user@host` and `user:pass@host`. A named setting changes only its
option, so prefer it for values supplied by less-trusted callers.

Variables cannot replace the service before `://`, a setting name, or a
`tag:`/`tags:` value. Services and tags must be known before values are filled
in. Template variables apply only to YAML configuration; TEXT configuration
is unchanged.

## CLI Tagging Support

Apprise allows you to tag your services in your configuration to organize them (e.g., `family`, `devops`, `critical`). You can then filter which services to notify using the `--tag` (`-g`) switch. Without `--tag`, only the untagged services are notified; `--tag=all` notifies everything. Tags only apply to configuration files: URLs given on the command line are always notified.

It is important to understand how Apprise handles multiple tags:

* **OR Logic (Union)**: To notify services that have *either* Tag A **OR** Tag B, specify the `-g` switch multiple times.
* **AND Logic (Intersection)**: To notify services that have *both* Tag A **AND** Tag B, separate the tags with a comma.

```bash
# OR Logic: Notify any service tagged 'devops' OR 'admin'
apprise -vv -t "Union Test" \
   --config=~/apprise.yml \
   -g devops -g admin

# AND Logic: Notify only services tagged with BOTH 'devops' AND 'critical'
apprise -vv -t "Intersection Test" \
   --config=~/apprise.yml \
   -g devops,critical
```

Tags also support an optional **priority prefix** and **retry suffix**. In your configuration file you assign a priority to a tag like `1:alerts` or `5:alerts`. A lower number means higher urgency.

* **Escalation (no prefix)**: `-g alerts` dispatches priority-1 entries first. If they all succeed, Apprise returns early and never runs the priority-5 fallbacks. A failure in the lower-priority group triggers the next group as an escalation chain.
* **Exclusive (with prefix)**: `-g "2:alerts"` notifies *only* services whose `alerts` tag has priority 2. No other priority levels are triggered.
* **Per-call retry**: `-g "alerts:3"` retries each matched service up to 3 times on failure (overrides the service's own retry setting for this call only).
* **Combined**: `-g "2:alerts:3"` selects only priority-2 services and allows up to 3 retries.

```bash
# Escalation: priority-1 first; skip priority-5 if all succeed
apprise -vv -t "Alert" --config=~/apprise.yml -g alerts

# Exclusive: only priority-2 alert services
apprise -vv -t "Alert" --config=~/apprise.yml -g "2:alerts"

# With retry override: retry each matched service up to 3 times
apprise -vv -t "Alert" --config=~/apprise.yml -g "alerts:3"
```

## CLI File Attachments

Apprise also supports file attachments too! Specify as many attachments to a notification as you want.

```bash
# Send a funny image you found on the internet to a colleague:
apprise -vv --title 'Agile Joke' \
        --body 'Did you see this one yet?' \
        --attach https://i.redd.it/my2t4d2fx0u31.jpg \
        'mailto://myemail:mypass@gmail.com'

# Easily send an update from a critical server to your dev team
apprise -vv --title 'system crash' \
        --body 'I do not think Jim fixed the bug; see attached...' \
        --attach /var/log/myprogram.log \
        --attach /var/debug/core.2345 \
        --tag devteam
```

An attachment can be a local path, a `file://` URL or an `http(s)://` URL; add `?name=` to a web attachment to rename it. A service that takes attachments fails if one cannot be read.

## CLI Environment Variables

Those using the Command Line Interface (CLI) can also leverage environment variables to pre-set the default settings:

| Variable                | Description       |
|------------------------ | ----------------- |
| `APPRISE_URLS`          |  Specify the default URLs to notify IF none are otherwise specified on the command line explicitly. If the `--config` (`-c`) is specified, then this will overrides any reference to this variable. Use white space and/or a comma (`,`) to delimit multiple entries.
|  `APPRISE_CONFIG_PATH`  | Explicitly specify the config search path to use (overriding the default). The path(s) defined here must point to the absolute filename to open/reference. Use a semi-colon (`;`), line-feed (`\n`), and/or carriage return (`\r`) to delimit multiple entries.
|  `APPRISE_STORAGE_PATH` | Explicitly specify the persistent storage path to use (overriding the default). `--storage-path` (`-S`) takes priority over it.
|  `APPRISE_STORAGE_PRUNE_DAYS` | The default for `--storage-prune-days` (`-SPD`), 30 if unset.
|  `APPRISE_STORAGE_UID_LENGTH` | The default for `--storage-uid-length` (`-SUL`), 8 if unset.
|  `APPRISE_TEMPLATE_<NAME>` | Supply a value for a `${NAME}` used by a YAML configuration. For example `APPRISE_TEMPLATE_API_KEY` fills in `${API_KEY}`. A value passed with `--template-var` (`-tv`) takes priority over this.
|  `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` | Route outbound HTTP requests through a proxy, or exempt hosts from it. SOCKS proxies are not supported.

## CLI Exit Status

| Status | Meaning |
| ------ | ------- |
| `0` | Every notification was sent. |
| `1` | Every notification failed, or none could start (for example, no services were loaded). |
| `2` | The command line was invalid, such as an unknown option or value. |
| `3` | Services were loaded, but the tags matched none of them. |
| `4` | At least one notification was sent and at least one was not (it failed or ran out of time). |
| `5` | Nothing was sent, and the only problem was that services ran out of time (`--limit`, `--service-limit`). |

# Developer API Usage

To send a notification from within your Rust application, add `apprise-core` (and an async runtime such as `tokio`) to your `Cargo.toml` and do the following:
```toml
[dependencies]
apprise-core = { git = "https://github.com/ENK0DED/apprise" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use apprise_core::{Apprise, NotifyContext};

#[tokio::main]
async fn main() {
  // Create an Apprise instance
  let mut apprise = Apprise::new();

  // Add notification services by URL; add() returns false if a URL
  // could not be loaded. A sample email notification:
  apprise.add("mailto://myuserid:mypass@gmail.com");

  // A sample pushbullet notification
  apprise.add("pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b");

  // Then notify these services any time you desire. The below would
  // notify all of the services loaded into our Apprise object.
  let ctx = NotifyContext {
    title: "my notification title".into(),
    body: "what a great notification service!".into(),
    ..Default::default()
  };
  let result = apprise.notify(&ctx).await;
  println!("{}: {} of {} sent", result.status, result.succeeded, result.total);
}
```

`NotifyContext` also sets the notification type (`notify_type: NotifyType::Warning`), the input format (`body_format: NotifyFormat::Markdown`), and backslash-escape and `:emoji:` interpretation. `notify()` returns a `SendResult`: the overall `status` (a `ResultStatus` whose `exit_code()` is the CLI's exit status), the `total`, `succeeded` and `failed` counts, and one `ServiceResult` per service in `results`.

## API Configuration Files

Developers need access to configuration files too. `add_config()` loads one from a path or a `file://`, `http://` or `https://` URL, and you can freely mix and match configuration and notification entries as often as you wish! You can read more about the expected structure of the configuration files [here](https://appriseit.com/getting-started/configuration/).
```rust
use apprise_core::{Apprise, ConfigError, NotifyContext, TagFilter};

async fn notify_admins() -> Result<(), ConfigError> {
  let mut apprise = Apprise::new();

  // Add a configuration source; the second argument is how many levels
  // of `include` to follow:
  apprise.add_config("/path/to/my/config.yml", 1).await?;

  // Add another...
  apprise.add_config("https://myserver:8080/path/to/config", 1).await?;

  // You can mix and match; add an entry directly if you want too.
  // In this entry we associate the 'admin' tag with our notification
  apprise.add_with_tags("mailto://myuser:mypass@hotmail.com", &["admin"]);

  let ctx = NotifyContext {
    title: "Attention Admins".into(),
    body: "send a notification to our admin group".into(),
    ..Default::default()
  };

  // notify() reaches absolutely everything loaded, regardless of
  // whether it has a tag associated with it or not (upstream's tag="all")
  apprise.notify(&ctx).await;

  // Tagging allows you to specifically target only specific notification
  // services you've loaded. TagFilter::from_cli() reads tags the way
  // --tag does: each entry is OR'd, commas inside one entry mean AND.
  let admins = TagFilter::from_cli(&["admin".to_string()]);
  apprise.notify_tagged(&ctx, &admins, &[]).await;
  Ok(())
}
```

`set_tag_strings()` or `set_tag_filter()` changes which services `notify()` reaches from then on, and `TagFilter::Untagged` selects only the untagged ones, as the CLI does without `--tag`.

## API Template Variables

After loading a templated YAML configuration, developers can supply its values
before sending. Services waiting for a value are kept aside
(`pending_templates()`) and load as soon as it is supplied:

```rust
use std::collections::HashMap;

apprise.set_template_values(HashMap::from([("api_key".to_string(), "your-secret-key".to_string())]))?;
```

These values take priority over `APPRISE_TEMPLATE_<NAME>` environment
variables and defaults in the YAML file. Names are case-insensitive, and extra
names are accepted and ignored.

Templates are enabled by default. Call `apprise.set_allow_templates(false)`
before loading the configuration to turn them off: the `template:` section and
template environment variables are then ignored, and `${NAME}` remains
unchanged. A configuration cannot enable this feature itself.

## API File Attachments

Attachments are very easy to send using `notify_with_attachments()`. Each source is a local path, a `file://` URL or an `http(s)://` URL:
```rust
let ctx = NotifyContext {
  title: "A great photo of our family".into(),
  body: "The flash caused Jane to close her eyes! hah! :)".into(),
  ..Default::default()
};

let attach = vec![
  "/local/path/to/my/DSC_003.jpg".to_string(),
  // ?name= allows us to rename the actual jpeg as found on the site
  // to be another name when sent to our recipient(s)
  "https://i.redd.it/my2t4d2fx0u31.jpg?name=FlyingToMars.jpg".to_string(),
];
apprise.notify_with_attachments(&ctx, &attach).await;
```

`notify_tagged()` takes the same list as its last argument. As upstream does, an attachment that can never load (an unknown schema, for example) fails the whole call, while one that cannot be read right now only fails the services that take attachments; the others send the message without it.

## Node.js API

The Node.js package exposes the same library:
```js
import { Apprise, send, listServices, parseUrl } from '@enk0ded/apprise';

const apprise = new Apprise();

// Add services by URL; add() returns false if a URL could not be loaded
apprise.add('mailto://myuserid:mypass@gmail.com');

// Load a configuration file or URL; resolves to the number of services loaded
await apprise.addConfig('/path/to/my/config.yml');

// Without tag/tags every loaded service is notified
const result = await apprise.notify('what a great notification service!', {
  title: 'my notification title',
  notificationType: 'success', // info, success, warning or failure
  bodyFormat: 'text', // text, markdown or html
  // Repeat entries to match ANY tag; use 'a,b' in one entry to require ALL
  tags: ['admin'],
  attach: ['/local/path/to/my/DSC_003.jpg'],
});
console.log(result); // { total, succeeded, failed, status }

// Or send in one call
await send({ urls: 'pbul://o.gn5kj6nfhv736I7jC3cj3QLRiyhgl98b', title: 'Test', body: 'Hello' });

// Every supported service, and what a URL would load
console.log(listServices().length, parseUrl('ntfy://mytopic')?.name);
```

`status` is one of `"success"`, `"failure"`, `"partial"`, `"nomatch"` or `"timeout"`. The TypeScript definitions in [`index.d.ts`](crates/apprise-napi/index.d.ts) document every option.

# Persistent Storage

Persistent storage allows Apprise to cache re-occurring actions optionally to disk, such as login tokens and looked-up IDs. This can greatly reduce the overhead used to send a notification. The directory layout and cache format match upstream's, so both can share a storage directory.

There are 3 Persistent Storage operational states Apprise can operate using:
1. `auto`: Flush gathered cache information to the filesystem on demand. This option is incredibly light weight. This is the default behavior for all CLI usage.
2. `flush`: Flushes any cache information to the filesystem during every transaction.
3. `memory`: Effectively disable Persistent Storage. Any caching of data required by each plugin used is done in memory. This setting ensures no content is ever written to disk.
   * The library operates this way until you give it a storage path, as explained further on in this documentation.

A single service can opt out with `?store=no` in its URL.

## CLI Persistent Storage Commands

You can provide the keyword `storage` on your CLI call to see the persistent storage options available to you.
```bash
# List all of the occupied space used by Apprise's Persistent Storage:
apprise storage list

# list is the default option, so the following does the same thing:
apprise storage

# You can prune all of your storage older then 30 days
# and not accessed for this period like so:
apprise storage prune

# You can do a hard reset (and wipe all persistent storage) with:
apprise storage clean
```

You can also filter your results by adding tags and/or URL Identifiers. When you get a listing (`apprise storage list`), you may see:
```
   # example output of 'apprise storage list':
   1. f7077a65                                             0.00B    unused
      - matrixs://abcdef:****@synapse.example12.com/%23general?image=no&mode=off&version=3&msgtype...
      tags: team

   2. 0e873a46                                            81.10B    active
      - tgram://W...U//?image=False&detect=yes&silent=no&preview=no&content=before&mdv=v1&format=m...
      tags: personal

   3. abcd123                                             12.00B    stale

```
The (persistent storage) cache states are:
 - `unused`: This plugin has not committed anything to disk for reuse/cache purposes
 - `active`: This plugin has written content to disk. Or at the very least, it has prepared a persistent storage location it can write into.
 - `stale`: The system detected a location where a URL may have possibly written to in the past, but there is nothing linking to it using the URLs provided. It is likely wasting space or is no longer of any use.

You can use this information to filter your results by specifying _URL ID_ (UID) values or full service URLs after your command. For example:
```bash
# The below commands continue with the example already identified above
# the following would match abcd123 (even though just ab was provided)
# The output would only list the 'stale' entry above
apprise storage list ab

# knowing our filter is safe, we could remove it
# the below command would not obstruct our other to URLs and would only
# remove our stale one:
apprise storage clean ab

# Entries can be filtered by tag as well:
apprise storage list --tag=team

# You can match on multiple URL ID's as well:
# The following would actually match the URL ID's of 1. and 2. above
apprise storage list f 0

# A service URL shows its UID, even before anything was written:
apprise storage list 'mailtos://user:pass@example.com'
```
When using the CLI, Persistent storage is set to the operational mode of `auto` by default, you can change this by providing `--storage-mode=` (`-SM`) during your calls. The storage lives in `~/.local/share/apprise/cache` (`%APPDATA%/Apprise/cache` on Windows) unless `--storage-path` (`-S`) or `APPRISE_STORAGE_PATH` says otherwise.

For more information on persistent storage, [visit here](https://appriseit.com/cli/persistent-storage/).

## API Persistent Storage

For developers, persistent storage stays in memory until the `AppriseAsset` given to `Apprise::with_asset()` has a storage path:
```rust
use apprise_core::{Apprise, AppriseAsset, StorageMode};

let asset = AppriseAsset {
  // Set our storage path directory (minimum requirement to enable it);
  // the mode is auto unless set otherwise
  storage_path: Some("/path/to/save/data".into()),

  // Set the mode... the options are:
  // 1. StorageMode::Memory - disable persistent storage from writing to disk
  // 2. StorageMode::Auto   - write to disk on demand
  // 3. StorageMode::Flush  - write to disk always and often
  storage_mode: StorageMode::Flush,

  // The URL IDs are by default 8 characters in length. You can increase and
  // decrease its value here. The value must be 2 or more.
  storage_idlen: 8,

  ..Default::default()
};

// Now that we've got our asset, we just work with our Apprise object as we
// normally do
let apprise = Apprise::with_asset(asset);
```

Stores flush when the `Apprise` instance is dropped; call `apprise.flush_stores()` before exiting without unwinding (for example with `std::process::exit`).

For more information on persistent storage, [visit here](https://appriseit.com/library/persistent-storage/).

# Differences from Upstream

This port aims to behave like upstream Apprise 2.1. Where it does not:

* **Not ported**: upstream's `url()` serialisation (turning a loaded service back into a URL, including the privacy URL; `--dry-run` and `storage` listings mask the URL you gave instead, hiding the user, password, path, query values and token-like hosts), and custom Python plugins (the `@notify` decorator, `--plugin-path` and `APPRISE_PLUGIN_PATH`, which the CLI accepts but ignores with a warning).
* **Time zones** (`?tz=`, YAML `asset: timezone:`) come from the system's tz database on Linux and macOS (`$TZDIR`, else `/usr/share/zoneinfo`; small container images may need the `tzdata` package) and from a copy built into Windows binaries. Where one location names several zones (`Pacific`, on systems that keep the old `Canada/` and `US/` links), the first in alphabetical order is used; upstream's pick is arbitrary.
* **YAML values**: a number given as `user`, `password` or `host` (`password: 123456`) is used as its text, where upstream cannot load the entry. A YAML 1.1 timestamp (`2001-12-14 21:59:43`) stays the text as written, where upstream passes a `datetime` on.
* **Persistent storage** holds at most 1 MiB of JSON per service: a value that would take it past that is refused and the previous one kept. Upstream has no limit; a larger cache file it wrote is left in place, unused.
* **Known gaps** in this port:
  * YAML settings that name targets (`to:`, ntfy's `topic:` and the like) are added to the targets the URL already names; upstream replaces them.
  * Storage IDs (the `uid` in listings, and the persistent store's namespace) match upstream for every service that stores something. About 125 services that store nothing still use the generic ID, so only their listed `uid` differs.
  * Markdown with tens of thousands of nested emphasis markers converts slowly (the time grows with the square of the input); ordinary messages are unaffected.
  * Library callers: `NotifyContext::default()` declares the body as text, where upstream's `notify()` passes an undeclared body through unconverted. Set `body_passthrough` for upstream's behaviour; the Node.js binding does when `bodyFormat` is omitted.
* **Shared with upstream, not fixed**:
  * Two processes (or two `Apprise` instances) that share a storage path and service overwrite each other's persistent-store changes; the last to write wins.
  * HTTP response bodies are read whole, so a hostile or broken server can make one send buffer as much as it sends.
* **Upstream bugs fixed deliberately** (several following the Go port, [apprise-go](https://github.com/unraid/apprise-go)):
  * Matrix: every event gets its own transaction ID, so Synapse no longer drops repeats; a room whose encryption cannot be confirmed, or an encrypted room in a build without `matrix-e2ee`, receives nothing rather than plaintext.
  * Viber: `?to=` is split into receiver IDs, not single characters.
  * WPush: redirects are never followed (the API key travels in the request), and only an explicit `code` of `0` counts as success.
  * LaMetric: cloud requests send the app's access token.
  * Pushbullet: file pushes go to the recipient, not to every device.
  * Twist and Reddit: after logging in again, the retried request uses the new token (upstream resends the refused one).
  * Office 365, Mailgun, SendGrid and SparkPost: one recipient's Cc and Bcc lists no longer carry over into the next recipient's request.
  * Telegram: each chat keeps its own topic when attachments are sent to several chats.
  * Slack: native `https://hooks.slack-gov.com/...` URLs keep their query arguments.
  * Pushover: attachment names are encrypted along with the rest of an encrypted message.
  * Home Assistant: the targets given after a service (`notify.mobile:device1`) stay with that service.
  * Synology Chat: the token and path are split by path segment, so an encoded token or a trailing `/` no longer breaks the request URL.
  * Plivo: requests go to the account's Auth ID, not to the literal `{auth_id}` URL template.
  * Pingram: every phone number keeps its leading `+`.
  * YAML: an `asset: timezone:` that is a path (`/etc/localtime`) is ignored with a warning; upstream raises out of the configuration loader.
  * `smpps://`: upstream connects without TLS (smpplib is given no `ssl_context`, so only the default port changes) and sends the system ID and password in the clear. The port runs the session over TLS, verified unless `?verify=no`, and fails the send if the handshake fails.
  * IRC: upstream builds its `PASS`, `NICK` and `USER` lines from the nickname, `?name=`, passwords and ZNC user as given, so a decoded `%0D%0A` injects commands. The port refuses a URL whose fields contain CR, LF or NUL, drops such targets, and refuses to write any line that contains one.
  * XMPP: upstream (slixmpp) logs SASL stanzas whole at DEBUG, including the base64 PLAIN payload, which decodes to the username and password. The port's log shows the element and mechanism, with the payload replaced by its length.
  * IRC, XMPP, SMTP and MQTT: upstream buffers whatever the server sends (an IRC line that never ends, an unfinished XMPP stanza, endless SMTP continuation lines, an MQTT packet of up to 256 MiB). The port fails the send past 8 KiB per IRC line, 1 MiB per XMPP stanza, 1,000 lines or 64 KiB per SMTP reply and 1 MiB per MQTT packet.
  * `syslog://`: upstream's `syslog()` call blocks without limit when the local logger is stuck, so the send hangs, and with a time limit it leaves a thread behind that blocks shutdown. The port bounds the connect and the write by `?cto=` and `?rto=` (4 s each by default), fails the send, and leaves no thread behind.
  * Storage prune: from about 739,000 days, `--storage-prune-days` (or `APPRISE_STORAGE_PRUNE_DAYS`) makes upstream crash with an `OverflowError`. The port prunes nothing, since no file is that old.
  * Slack (legacy API, Markdown with no declared format): upstream reads the text of `<!…>`, `<@…>` and `<url|…>` links as a `re.sub()` template, so backslashes are interpreted, `\d` or `\1` fail the send, and each link overwrites copies of itself written in another case. The port sends each link as written.
  * Slack (legacy API): upstream fails any send whose title, or whose Markdown body with no declared format, contains CR `*` LF (a `KeyError` in its escaping table). The port escapes only `&`, `<` and `>`, and sends it.
  * Telegram (HTML with no declared format): upstream's comment pattern starts at any `<!`, so a document with `<!DOCTYPE html>` and a later comment loses everything between them. The port removes only the comments, ending them where HTML does, and removes the doctype on its own.

# Want To Learn More?

If you're interested in reading more about this and other methods on how to customize your own notifications, please check out upstream's documentation, which this port follows:
* 📣 [Using the CLI](https://appriseit.com/cli/)
* ⚙️ [Configuration File Help](https://appriseit.com/getting-started/configuration/)
* 🌎 [Apprise API/Web Interface](https://github.com/caronc/apprise-api/)
* 📖 [Apprise Documentation Source](https://github.com/caronc/apprise-docs/)
* 🔧 [Troubleshooting](https://appriseit.com/qa/)
* 🐍 [Upstream Apprise (Python)](https://github.com/caronc/apprise)

Want to help make this port better? See [CONTRIBUTING.md](CONTRIBUTING.md). Want to support the project it is built on?
* ❤️ [Sponsor or donate to upstream Apprise](https://appriseit.com/contributing/sponsors/)
