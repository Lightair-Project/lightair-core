//! Streaming platforms for destinations: where each takes a stream, where its key lives, its shape
//! and its limits. The table mirrors the desktop's
//! (`lightair/crates/lightair-app/src/platforms.rs`, whose ingest servers are the platforms'
//! published ones); keep them in step until it moves to lightair-core. Platforms that hand every
//! account its own server (Kick, TikTok, Instagram, LinkedIn) ask for it to be pasted.

/// Where a platform is in the picker: the ones most people use, the rest worldwide, then by
/// country (the viewer's own first), then a custom server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Region {
    Popular,
    More,
    Korea,
    Japan,
    China,
    Other,
}

impl Region {
    pub fn title(self) -> &'static str {
        match self {
            Region::Popular => "Popular",
            Region::More => "More platforms",
            Region::Korea => "Korea · 한국",
            Region::Japan => "Japan · 日本",
            Region::China => "China · 中国",
            Region::Other => "Other",
        }
    }
}

/// A platform's preferred shape.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shape {
    Landscape,
    Vertical,
}

pub struct Platform {
    /// Stored on the destination (`Destination::platform`).
    pub id: &'static str,
    pub name: &'static str,
    /// Its name in its own language ("" when it's the same), shown under the name and searched.
    pub native: &'static str,
    /// Other names people search for (old names, short names).
    pub aliases: &'static [&'static str],
    pub region: Region,
    /// Badge: brand colour, and the logo (an icon name) or else a short mark.
    pub color: u32,
    pub mark: &'static str,
    pub logo: Option<&'static str>,
    /// (label, url). Empty: the account's own server is pasted in.
    pub servers: &'static [(&'static str, &'static str)],
    /// Where to find the server (when pasted) and the key.
    pub help: &'static str,
    pub key_link: Option<&'static str>,
    pub shape: Shape,
    /// Highest video bitrate the platform takes, kbps.
    pub max_kbps: Option<u32>,
}

pub const TWITCH_SERVERS: &[(&str, &str)] = &[
    (
        "Asia Pacific (Mumbai)",
        "rtmp://aps30.contribute.live-video.net/app/",
    ),
    (
        "Asia Pacific (Seoul)",
        "rtmp://apn20.contribute.live-video.net/app/",
    ),
    (
        "Asia Pacific (Singapore)",
        "rtmp://aps10.contribute.live-video.net/app/",
    ),
    (
        "Asia Pacific (Sydney)",
        "rtmp://aps20.contribute.live-video.net/app/",
    ),
    (
        "Asia Pacific (Tokyo)",
        "rtmp://apn10.contribute.live-video.net/app/",
    ),
    (
        "Europe (Frankfurt)",
        "rtmp://euc10.contribute.live-video.net/app/",
    ),
    (
        "Europe (Ireland)",
        "rtmp://euw10.contribute.live-video.net/app/",
    ),
    (
        "Europe (Paris)",
        "rtmp://euw30.contribute.live-video.net/app/",
    ),
    (
        "Europe (Stockholm)",
        "rtmp://eun10.contribute.live-video.net/app/",
    ),
    (
        "South America (São Paulo)",
        "rtmp://sae10.contribute.live-video.net/app/",
    ),
    (
        "US East (N. Virginia)",
        "rtmp://use10.contribute.live-video.net/app/",
    ),
    (
        "US East (Ohio)",
        "rtmp://use20.contribute.live-video.net/app/",
    ),
    (
        "US West (Oregon)",
        "rtmp://usw20.contribute.live-video.net/app/",
    ),
];

pub const PLATFORMS: &[Platform] = &[
    Platform {
        id: "youtube",
        name: "YouTube",
        native: "",
        aliases: &[],
        region: Region::Popular,
        color: 0xFF0033,
        mark: "YT",
        logo: Some("platforms/youtube"),
        servers: &[
            ("Primary", "rtmps://a.rtmps.youtube.com:443/live2"),
            ("Backup", "rtmps://b.rtmps.youtube.com:443/live2?backup=1"),
        ],
        help: "YouTube Studio → Go live → Stream: copy the stream key.",
        key_link: Some("https://www.youtube.com/live_dashboard"),
        shape: Shape::Landscape,
        max_kbps: Some(51_000),
    },
    Platform {
        id: "youtube-shorts",
        name: "YouTube Shorts",
        native: "",
        aliases: &["shorts"],
        region: Region::Popular,
        color: 0xFF0033,
        mark: "YS",
        logo: Some("platforms/youtubeshorts"),
        servers: &[
            ("Primary", "rtmps://a.rtmps.youtube.com:443/live2"),
            ("Backup", "rtmps://b.rtmps.youtube.com:443/live2?backup=1"),
        ],
        help: "A second YouTube stream, vertical, shown as a Short. In YouTube Studio → Go live → Stream, make another stream key for it (one channel can stream to several keys at once).",
        key_link: Some("https://www.youtube.com/live_dashboard"),
        shape: Shape::Vertical,
        max_kbps: Some(51_000),
    },
    Platform {
        id: "twitch",
        name: "Twitch",
        native: "",
        aliases: &[],
        region: Region::Popular,
        color: 0x9146FF,
        mark: "Tw",
        logo: Some("platforms/twitch"),
        servers: TWITCH_SERVERS,
        help: "Creator Dashboard → Settings → Stream: copy the primary stream key.",
        key_link: Some("https://dashboard.twitch.tv/settings/stream"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "facebook",
        name: "Facebook",
        native: "",
        aliases: &["meta"],
        region: Region::Popular,
        color: 0x1877F2,
        mark: "f",
        logo: Some("platforms/facebook"),
        servers: &[("Default", "rtmps://rtmp-api.facebook.com:443/rtmp/")],
        help: "Live Producer → Streaming software: copy the stream key.",
        key_link: Some("https://www.facebook.com/live/producer"),
        shape: Shape::Landscape,
        max_kbps: Some(9_000),
    },
    Platform {
        id: "kick",
        name: "Kick",
        native: "",
        aliases: &[],
        region: Region::Popular,
        color: 0x53FC18,
        mark: "K",
        logo: Some("platforms/kick"),
        servers: &[],
        help: "Kick dashboard → Settings → Stream: copy the Stream URL and Stream Key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: Some(8_000),
    },
    Platform {
        id: "tiktok",
        name: "TikTok",
        native: "",
        aliases: &[],
        region: Region::Popular,
        color: 0xFE2C55,
        mark: "Tk",
        logo: Some("platforms/tiktok"),
        servers: &[],
        help: "TikTok LIVE Producer (on accounts that have it): copy the Server URL and Stream Key.",
        key_link: None,
        shape: Shape::Vertical,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "instagram",
        name: "Instagram",
        native: "",
        aliases: &["ig", "meta"],
        region: Region::Popular,
        color: 0xE1306C,
        mark: "IG",
        logo: Some("platforms/instagram"),
        servers: &[],
        help: "Instagram on the web → Create → Live video: copy the Stream URL and Stream key.",
        key_link: None,
        shape: Shape::Vertical,
        max_kbps: Some(4_000),
    },
    Platform {
        id: "x",
        name: "X",
        native: "",
        aliases: &["twitter", "periscope"],
        region: Region::Popular,
        color: 0x111111,
        mark: "X",
        logo: Some("platforms/x"),
        servers: &[
            ("US West: California", "rtmp://ca.pscp.tv:80/x"),
            ("US West: Oregon", "rtmp://or.pscp.tv:80/x"),
            ("US East: Virginia", "rtmp://va.pscp.tv:80/x"),
            ("South America: Brazil", "rtmp://br.pscp.tv:80/x"),
            ("EU West: France", "rtmp://fr.pscp.tv:80/x"),
            ("EU West: Ireland", "rtmp://ie.pscp.tv:80/x"),
            ("EU Central: Germany", "rtmp://de.pscp.tv:80/x"),
            ("Asia/Pacific: Australia", "rtmp://au.pscp.tv:80/x"),
            ("Asia/Pacific: India", "rtmp://in.pscp.tv:80/x"),
            ("Asia/Pacific: Japan", "rtmp://jp.pscp.tv:80/x"),
            ("Asia/Pacific: Korea", "rtmp://kr.pscp.tv:80/x"),
            ("Asia/Pacific: Singapore", "rtmp://sg.pscp.tv:80/x"),
        ],
        help: "Media Studio → Producer → Sources: create a source and copy its stream key.",
        key_link: Some("https://studio.twitter.com/producer/sources"),
        shape: Shape::Landscape,
        max_kbps: Some(12_000),
    },
    Platform {
        id: "linkedin",
        name: "LinkedIn",
        native: "",
        aliases: &[],
        region: Region::More,
        color: 0x0A66C2,
        mark: "in",
        logo: Some("platforms/linkedin"),
        servers: &[],
        help: "LinkedIn Live event → Stream settings: copy the Stream URL and Stream key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    // Korea (Twitch left in February 2024)
    Platform {
        id: "chzzk",
        name: "CHZZK",
        native: "치지직",
        aliases: &["chijijik", "naver"],
        region: Region::Korea,
        color: 0x00FFA3,
        mark: "CZ",
        logo: None,
        servers: &[(
            "Default",
            "rtmp://global-rtmp.lip2.navercorp.com:8080/relay",
        )],
        help: "CHZZK 스튜디오 → 방송 설정 (studio.chzzk.naver.com): copy the 스트림 키. Once Lightair is live, press start in the studio. Keyframes every second, no B-frames.",
        key_link: Some("https://studio.chzzk.naver.com"),
        shape: Shape::Landscape,
        max_kbps: Some(8_000),
    },
    Platform {
        id: "soop",
        name: "SOOP",
        native: "숲",
        aliases: &["afreeca", "afreecatv", "아프리카"],
        region: Region::Korea,
        color: 0x0052FF,
        mark: "S",
        logo: None,
        servers: &[("Default", "rtmp://stream.soop.live/app/")],
        help: "SOOP (formerly AfreecaTV) → your channel → 외부장치 방송 설정 (External Device Broadcast Settings): copy the Stream Name (스트림 키).",
        key_link: Some("https://dashboard.sooplive.co.kr"),
        shape: Shape::Landscape,
        max_kbps: Some(8_000),
    },
    Platform {
        id: "pandatv",
        name: "PandaTV",
        native: "팬더티비",
        aliases: &["panda"],
        region: Region::Korea,
        color: 0x1F8FFF,
        mark: "PD",
        logo: None,
        servers: &[("Default", "rtmp://rtmp.pandalive.co.kr/app")],
        help: "PandaTV (팬더티비) studio → broadcast settings: copy the stream key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: Some(7_500),
    },
    Platform {
        id: "cime",
        name: "CIME",
        native: "씨미",
        aliases: &[],
        region: Region::Korea,
        color: 0x7B5CFF,
        mark: "CI",
        logo: None,
        servers: &[],
        help: "CIME (씨미) studio → broadcast settings: copy the Stream URL and Stream key.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: None,
    },
    // Japan
    Platform {
        id: "niconico",
        name: "Niconico",
        native: "ニコニコ生放送",
        aliases: &["nico", "nicolive", "ニコ生"],
        region: Region::Japan,
        color: 0x231815,
        mark: "ニ",
        logo: Some("platforms/niconico"),
        servers: &[("Default", "rtmp://liveorigin.dlive.nicovideo.jp/live/input")],
        help: "ニコニコ生放送: 放送する → 放送を開始する → choose the web-browser option, then copy the 「ストリームキー」. The key is new for every broadcast.",
        key_link: Some("https://live.nicovideo.jp/create"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "twitcasting",
        name: "TwitCasting",
        native: "ツイキャス",
        aliases: &["twitcas", "cas"],
        region: Region::Japan,
        color: 0x3381FF,
        mark: "TC",
        logo: None,
        servers: &[],
        help: "ツイキャス: 配信する → ツールとの連携設定, choose 画質, then copy the RTMP URL and ストリームキー (copy them again if you change 画質).",
        key_link: Some("https://twitcasting.tv/"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "showroom",
        name: "SHOWROOM",
        native: "ショールーム",
        aliases: &["sr", "showroom-live"],
        region: Region::Japan,
        color: 0x1DA1E5,
        mark: "SR",
        logo: None,
        servers: &[],
        help: "SHOWROOM: 配信する → 配信ツール (OBS) を選ぶ, then copy the server URL and the stream key it shows.",
        key_link: Some("https://www.showroom-live.com/"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "17live",
        name: "17LIVE",
        native: "イチナナ",
        aliases: &["17", "ichinana"],
        region: Region::Japan,
        color: 0xFF4A52,
        mark: "17",
        logo: None,
        servers: &[],
        help: "17LIVE (イチナナ) on the web → 配信 → title → URL作成: copy the PUSH URL and Push Key (not the PULL URL). Needs phone-number verification.",
        key_link: Some("https://jp.17.live"),
        shape: Shape::Vertical,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "mirrativ",
        name: "Mirrativ",
        native: "ミラティブ",
        aliases: &[],
        region: Region::Japan,
        color: 0x7DD0F3,
        mark: "Mr",
        logo: None,
        servers: &[],
        help: "ミラティブ on the web (mirrativ.com) → 配信する: copy the OBS用URL and キー. Mirrativ takes at most 720p, 30 fps, about 1 Mbps.",
        key_link: Some("https://www.mirrativ.com"),
        shape: Shape::Vertical,
        max_kbps: Some(1_500),
    },
    Platform {
        id: "whowatch",
        name: "Whowatch",
        native: "ふわっち",
        aliases: &["fuwacchi"],
        region: Region::Japan,
        color: 0xF9A11B,
        mark: "ふ",
        logo: None,
        servers: &[
            (
                "Global (RTMPS, recommended)",
                "rtmps://ingest.global-contribute.live-video.net:443/app",
            ),
            (
                "Global (RTMP)",
                "rtmp://ingest.global-contribute.live-video.net/app",
            ),
        ],
        help: "ふわっち: open the broadcast screen in the app and copy the stream key. No B-frames.",
        key_link: Some("https://whowatch.tv"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    // China
    Platform {
        id: "bilibili",
        name: "Bilibili",
        native: "哔哩哔哩直播",
        aliases: &["b站", "bili"],
        region: Region::China,
        color: 0x00A1D6,
        mark: "B",
        logo: Some("platforms/bilibili"),
        servers: &[
            (
                "Global: primary",
                "rtmp://live-push.bilivideo.com/live-bvc/",
            ),
            (
                "Outside mainland China: primary",
                "rtmp://bdy.live-push.bilivideo.com/live-bvc/",
            ),
            (
                "Mainland China: backup",
                "rtmp://txy2.live-push.bilivideo.com/live-bvc/",
            ),
            (
                "Outside mainland China: backup",
                "rtmp://txy.live-push.bilivideo.com/live-bvc/",
            ),
        ],
        help: "哔哩哔哩直播: link.bilibili.com → 直播中心 → 我的直播间 → 开播设置 → choose 分区 → 开始直播, then copy the 推流码. Needs real-name verification and 5,000 followers; below that, use 直播姬's third-party mode (第三方开播模式).",
        key_link: Some("https://link.bilibili.com/p/center/index#/my-room/start-live"),
        shape: Shape::Landscape,
        max_kbps: None,
    },
    Platform {
        id: "huya",
        name: "Huya",
        native: "虎牙直播",
        aliases: &[],
        region: Region::China,
        color: 0xFF9600,
        mark: "HY",
        logo: None,
        servers: &[("Default", "rtmp://al.direct.huya.com/huyalive/")],
        help: "虎牙: huya.com → 个人中心 → 主播设置 → 开通远程推流 → choose a category → 获取推流地址并开播, then copy everything after huyalive/ as the key. Turn VPNs off. Keyframes every 3 seconds.",
        key_link: Some("https://i.huya.com"),
        shape: Shape::Landscape,
        max_kbps: Some(6_000),
    },
    Platform {
        id: "douyu",
        name: "DouYu",
        native: "斗鱼",
        aliases: &[],
        region: Region::China,
        color: 0xFF552E,
        mark: "DY",
        logo: None,
        servers: &[],
        help: "斗鱼: 创作中心 → 开始直播 (or 斗鱼直播伴侣 → 第三方转推), then copy the RTMP address and 直播码. Copy them fresh every time you go live.",
        key_link: Some("https://www.douyu.com/creator"),
        shape: Shape::Landscape,
        max_kbps: None,
    },
    Platform {
        id: "wechat-channels",
        name: "WeChat Channels",
        native: "视频号",
        aliases: &["weixin", "wechat", "shipinhao"],
        region: Region::China,
        color: 0x07C160,
        mark: "视",
        logo: None,
        servers: &[],
        help: "视频号: channels.weixin.qq.com (视频号助手) → 直播 → 直播间管理 → turn on 推流, then 直播管理 → 创建: copy the 推流地址 and 推流密钥 (they expire after 2 hours or when the room ends). Needs a verified account, 500 followers, 10 videos and 3 past lives.",
        key_link: Some("https://channels.weixin.qq.com"),
        shape: Shape::Vertical,
        max_kbps: Some(3_000),
    },
    Platform {
        id: "xiaohongshu",
        name: "Xiaohongshu",
        native: "小红书",
        aliases: &["red", "rednote", "xhs"],
        region: Region::China,
        color: 0xFF2442,
        mark: "RED",
        logo: Some("platforms/xiaohongshu"),
        servers: &[("Default", "rtmp://live-push.xhscdn.com/live")],
        help: "小红书: in the app's pre-live screen tap 电脑 for a 6-digit code, enter it on the PC live page, then copy the 串流密钥. The code is new for every live.",
        key_link: None,
        shape: Shape::Vertical,
        max_kbps: None,
    },
    Platform {
        id: "rumble",
        name: "Rumble",
        native: "",
        aliases: &[],
        region: Region::More,
        color: 0x85C742,
        mark: "R",
        logo: Some("platforms/rumble"),
        servers: &[],
        help: "Rumble → Go live: create the stream, then Get Streamer Configuration: copy the Stream URL and Stream Key (live streaming needs a verified phone number).",
        key_link: Some("https://rumble.com/live"),
        shape: Shape::Landscape,
        max_kbps: None,
    },
    Platform {
        id: "restream",
        name: "Restream",
        native: "",
        aliases: &["multistream"],
        region: Region::More,
        color: 0x2C5BFF,
        mark: "Re",
        logo: None,
        servers: &[("Automatic", "rtmp://live.restream.io/live")],
        help: "Restream → Streaming setup: copy the stream key (Restream sends it on to your channels).",
        key_link: Some("https://restream.io/settings/streaming-setup"),
        shape: Shape::Landscape,
        max_kbps: None,
    },
    Platform {
        id: "custom",
        name: "Custom",
        native: "",
        aliases: &["rtmp", "srt", "rist", "whip", "server"],
        region: Region::Other,
        color: 0x3A3A42,
        mark: "··",
        logo: None,
        servers: &[],
        help: "Any rtmp(s)://, srt:// or rist:// address, or an https:// WHIP endpoint.",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: None,
    },
];

pub fn by_id(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().chain(SERVICES).find(|p| p.id == id)
}

/// Services events come from that aren't destinations (tips): badges in Chat and Activity.
pub const SERVICES: &[Platform] = &[
    service("streamlabs", "Streamlabs", 0x80F5D2, "SL"),
    service("streamelements", "StreamElements", 0x1B1F2A, "SE"),
    service("kofi", "Ko-fi", 0xFF5E5B, "Ko"),
];

const fn service(id: &'static str, name: &'static str, color: u32, mark: &'static str) -> Platform {
    Platform {
        id,
        name,
        native: "",
        aliases: &[],
        region: Region::Other,
        color,
        mark,
        logo: None,
        servers: &[],
        help: "",
        key_link: None,
        shape: Shape::Landscape,
        max_kbps: None,
    }
}

/// The server address's host ("a.rtmps.youtube.com" in "rtmps://a.rtmps.youtube.com:443/live2").
fn host(server: &str) -> String {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    host.split(':').next().unwrap_or("").to_lowercase()
}

/// The platform a server address belongs to (for destinations added before platforms were
/// kept): by its host, one of the platform's listed servers or a domain the platform owns.
pub fn guess(server: &str) -> Option<&'static Platform> {
    let host = host(server);
    if host.is_empty() {
        return None;
    }
    if let Some(p) = PLATFORMS
        .iter()
        .find(|p| p.servers.iter().any(|(_, s)| self::host(s) == host))
    {
        return Some(p);
    }
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    // Kick's ingest is on the same network as Twitch's, under global-contribute.
    let id = if under("global-contribute.live-video.net") || under("kick.com") {
        "kick"
    } else if under("youtube.com") {
        "youtube"
    } else if under("live-video.net") || under("twitch.tv") {
        "twitch"
    } else if under("facebook.com") {
        "facebook"
    } else if under("pscp.tv") {
        "x"
    } else if under("rumble.com") {
        "rumble"
    } else if under("restream.io") {
        "restream"
    } else if under("tiktok.com") || under("tiktokcdn.com") {
        "tiktok"
    } else if under("instagram.com") {
        "instagram"
    } else if under("linkedin.com") {
        "linkedin"
    } else if under("navercorp.com") {
        "chzzk"
    } else if under("soop.live") || under("afreecatv.com") {
        "soop"
    } else if under("pandalive.co.kr") {
        "pandatv"
    } else if under("nicovideo.jp") {
        "niconico"
    } else if under("twitcasting.tv") {
        "twitcasting"
    } else if under("17.live") || under("17app.co") {
        "17live"
    } else if under("mirrativ.com") {
        "mirrativ"
    } else if under("bilivideo.com") {
        "bilibili"
    } else if under("huya.com") {
        "huya"
    } else if under("douyu.com") || under("douyucdn.cn") {
        "douyu"
    } else if under("weixin.qq.com") {
        "wechat-channels"
    } else if under("xhscdn.com") {
        "xiaohongshu"
    } else {
        return None;
    };
    by_id(id)
}

/// Whether a platform matches what's typed in the picker's search: its name, its native name,
/// its id or an alias, ignoring case.
pub fn matches(p: &Platform, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || [p.name, p.native, p.id]
            .iter()
            .chain(p.aliases)
            .any(|n| n.to_lowercase().contains(&q))
}

/// What a platform takes, where known from its published guidance (`None`: not limited here).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Limits {
    pub max_kbps: Option<u32>,
    pub max_height: Option<u32>,
    pub max_fps: Option<u32>,
    /// A keyframe at least this often (s).
    pub keyframe_s: Option<u32>,
}

pub fn limits(p: &Platform) -> Limits {
    let known = match p.id {
        "twitch" => (Some(6_000), Some(1080), Some(60)),
        "youtube" | "youtube-shorts" => (None, Some(2160), Some(60)),
        "facebook" => (Some(9_000), Some(1080), Some(60)),
        "kick" => (Some(8_000), Some(1080), Some(60)),
        _ => (None, None, None),
    };
    Limits {
        max_kbps: p.max_kbps.or(known.0),
        max_height: known.1,
        max_fps: known.2,
        // every platform here asks for one at least every 2 s (the engine's own interval)
        keyframe_s: Some(2),
    }
}

/// The platform whose sign-in can give the stream key, if any.
pub fn key_by_sign_in(p: &Platform) -> Option<&'static str> {
    match p.id {
        "twitch" => Some("twitch"),
        "youtube" | "youtube-shorts" => Some("youtube"),
        _ => None,
    }
}

/// The server chosen automatically: Twitch's nearest ingest, else the platform's first.
pub fn automatic_server(p: &Platform) -> Result<String, String> {
    if p.id == "twitch"
        && let Ok(i) = crate::ingest::twitch_nearest()
    {
        return Ok(i.url);
    }
    p.servers
        .first()
        .map(|(_, url)| url.to_string())
        .ok_or_else(|| {
            format!(
                "{} gives every account its own server: paste it from {}.",
                p.name,
                p.key_link.unwrap_or("the platform's settings")
            )
        })
}
