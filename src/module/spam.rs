//! Contains logic relating to calculating and tracking spam pressure.

use serenity::model::channel::{Message, MessageType};
use crate::dispatch::config::{VerifiedRole, ValueType, FromStrWithCtx};
use serenity::model::guild::Member;
use noisy_float::types::R64;
use std::{time, fmt};
use noisy_float::prelude::Float;
use crate::module::{Module, UnimplementedModule, ModInfo, Sensitivity};
use serenity::client::Context;

use crate::dispatch::Dispatch;
use once_cell::sync::Lazy;
use crate::dispatch::config;
use std::str::FromStr;
use std::fmt::Formatter;
use serenity::model::id::{GuildId, UserId};
use regex::Regex;
use dashmap::DashMap;
use std::sync::Arc;
use crate::db::cache::{TimedCache, Cache};
use byteorder::BigEndian;
use std::borrow::Cow;
use zerocopy::{AsBytes, U64};
use num::Zero;
use crate::util::clock::CacheInstant;
use crate::module::moderation::{MUTE_ROLE, ModAction, ActionKind};
use crate::error::{GuildNotInCache, LogErrorExt};
use crate::module::privilege::PRIV_ROLE;
use std::time::Duration;
use serenity::model::prelude::ReactionType::Unicode;

/// Base pressure generated by sending a message.
pub const DEFAULT_BASE_PRESSURE: f64 = 10.0;
/// Default pressure at which a user will be silenced.
pub const DEFAULT_MAX_PRESSURE: f64 = 60.0;
/// Default pressure for images.
pub const DEFAULT_IMAGE_PRESSURE: f64 = (DEFAULT_MAX_PRESSURE - DEFAULT_BASE_PRESSURE) / 6.0;
/// Default pressure for message length, per UTF-8 codepoint
pub const DEFAULT_LENGTH_PRESSURE: f64 = (DEFAULT_MAX_PRESSURE - DEFAULT_BASE_PRESSURE) / 8000.0;
/// Default pressure per line.
pub const DEFAULT_LINE_PRESSURE: f64 = (DEFAULT_MAX_PRESSURE - DEFAULT_BASE_PRESSURE) / 70.0;
/// Default pressure per ping.
pub const DEFAULT_PING_PRESSURE: f64 = (DEFAULT_MAX_PRESSURE - DEFAULT_BASE_PRESSURE) / 20.0;
/// Default pressure decay; this is the period in seconds for removal of one base pressure.
pub const DEFAULT_PRESSURE_DECAY: f64 = 2.5;
/// Default silence timeout; this the duration of any automutes Glimbot performs.
pub const DEFAULT_SILENCE_TIMEOUT: time::Duration = time::Duration::from_secs(10 * 60);

/// The config key for grabbing a [`SpamConfig`].
pub const SPAM_CONFIG_KEY: &str = "spam_config";
/// The config key for grabbing a role that should be immune to spam checks.
/// Guild owners and moderators cannot generate pressure.
pub const SPAM_IGNORE_ROLE: &str = "spam_ignore_role";

/// Matches known vertical whitespace characters; we count each of them as a "line" separator.
pub static VERTICAL_WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(
    r#"[\r\v\f\n\u2028\u2029]"#
).expect("Invalid vertical whitespace RE"));

/// The numerical configuration values for the spam module.
#[derive(Serialize, Deserialize, Copy, Clone)]
pub struct SpamConfig {
    /// Base pressure generated by sending a message.
    pub base_pressure: R64,
    /// Pressure generated by each image in a message.
    pub image_pressure: R64,
    /// Pressure generated per UTF-8 code point in a message.
    pub length_pressure: R64,
    /// Pressure generated per newline in a message.
    pub line_pressure: R64,
    /// The pressure at which a user will be silenced.
    pub max_pressure: R64,
    /// Pressure generated per ping in a message.
    pub ping_pressure: R64,
    /// The amount of time it will take for one `base_pressure` worth of pressure to decay.
    pub pressure_decay: R64,
    /// The amount of time users will be muted for.
    #[serde(with = "humantime_serde")]
    pub silence_timeout: time::Duration,
}

impl FromStr for SpamConfig {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl fmt::Display for SpamConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{}".to_string());
        write!(f, "{}", s)
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct UserPressure {
    last_update: CacheInstant,
    pressure: R64,
}

impl Default for UserPressure {
    fn default() -> Self {
        Self {
            last_update: CacheInstant::now(),
            pressure: R64::zero(),
        }
    }
}

impl UserPressure {
    pub fn update(mut self, new_pressure: R64, conf: &SpamConfig) -> UserPressure {
        // First apply the decay.
        if conf.pressure_decay != 0.0 && self.pressure != 0.0 {
            let elapsed = self.last_update.elapsed();
            let decay = R64::try_new(elapsed.as_secs_f64()).unwrap_or_else(R64::zero).raw() / conf.pressure_decay.raw().clamp(0.0, f64::MAX);
            let decay = decay * conf.base_pressure.raw();
            let new_pressure = (self.pressure.raw() - decay).clamp(0.0, f64::MAX);
            self.pressure = R64::new(new_pressure);
        }

        self.pressure = R64::new((self.pressure.raw() + new_pressure.raw()).clamp(0.0, f64::MAX));
        self.last_update = CacheInstant::now();
        self
    }
}


impl Default for SpamConfig {
    fn default() -> Self {
        Self {
            base_pressure: R64::new(DEFAULT_BASE_PRESSURE),
            image_pressure: R64::new(DEFAULT_IMAGE_PRESSURE),
            length_pressure: R64::new(DEFAULT_LENGTH_PRESSURE),
            line_pressure: R64::new(DEFAULT_LINE_PRESSURE),
            max_pressure: R64::new(DEFAULT_MAX_PRESSURE),
            ping_pressure: R64::new(DEFAULT_PING_PRESSURE),
            pressure_decay: R64::new(DEFAULT_PRESSURE_DECAY),
            silence_timeout: DEFAULT_SILENCE_TIMEOUT,
        }
    }
}

/// Calculates the message pressure of a single message.
pub fn message_pressure(conf: &SpamConfig, msg: &Message) -> R64 {
    let mut pres = conf.base_pressure.raw();

    // Add image pressure.
    pres += msg.attachments.iter()
        .filter_map(|a| a.height.map(|_| conf.image_pressure.raw()))
        .sum::<f64>();

    // Length pressure.
    pres += msg.content.len() as f64 * conf.length_pressure.raw();

    // Pings.
    pres += ((msg.mentions.len() + msg.mention_roles.len()) as f64 + msg.mention_everyone as u64 as f64) * conf.ping_pressure.raw();

    // Line pressure.
    pres += VERTICAL_WHITESPACE_RE.find_iter(&msg.content).count() as f64 * conf.line_pressure.raw();

    R64::try_new(pres).unwrap_or_else(R64::max_value)
}

/// Module containing the spam filtering logic for Glimbot.
pub struct SpamModule {
    cache: TimedCache<GuildId, SpamConfig>,
    user_pressure: Cache<GuildId, Cache<UserId, UserPressure>>,
}

impl Default for SpamModule {
    fn default() -> Self {
        Self {
            cache: TimedCache::new(std::time::Duration::from_secs(10)),
            user_pressure: Cache::null(),
        }
    }
}

#[async_trait::async_trait]
impl Module for SpamModule {
    fn info(&self) -> &ModInfo {
        #[doc(hidden)]
        static INFO: Lazy<ModInfo> = Lazy::new(|| {
            ModInfo::with_name("spam")
                .with_sensitivity(Sensitivity::High)
                .with_message_hook(true)
                .with_tick_hook(true)
                .with_command(true)
                .with_config_value(config::Value::<VerifiedRole>::new(SPAM_IGNORE_ROLE, "A role which should be ignored for spam pressure calculations. The guild owner and moderators will not generate pressure."))
                .with_config_value(config::Value::<SpamConfig>::with_default(SPAM_CONFIG_KEY, "A JSON object describing various options for calculating spam pressure. See Glimbot's documentation for more info.", Default::default))
        });
        &INFO
    }

    async fn process(&self, _dis: &Dispatch, _ctx: &Context, _orig: &Message, _command: Vec<String>) -> crate::error::Result<()> {
        Err(UnimplementedModule.into())
    }

    async fn on_tick(&self, _dis: &Dispatch, _ctx: &Context) -> crate::error::Result<()> {
        Ok(())
    }

    async fn on_message(&self, dis: &Dispatch, ctx: &Context, orig: &Message) -> crate::error::Result<()> {
        let gid = match orig.guild_id {
            None => {
                trace!("saw DM or other non-guild message");
                return Ok(());
            }
            Some(id) => { id }
        };

        let start = std::time::Instant::now();
        let f = async {
            let db = dis.db(gid);
            let v = dis.config_value_t::<SpamConfig>(SPAM_CONFIG_KEY).unwrap();
            Ok(*v.get_or_default(&db).await?)
        };
        let conf = self.cache.get_or_insert_with(&gid, f).await?;
        let pre_mess = start.elapsed();
        let lp = message_pressure(&conf, orig);

        let pres_cache = self.user_pressure.get_or_insert_default(&gid);
        let pres = pres_cache.update_and_fetch(&orig.author.id, |o| {
            let o = o.cloned().unwrap_or_else(Default::default);
            Some(o.update(lp, &conf))
        }).unwrap();

        if pres.pressure > conf.max_pressure {
            let r = mute_for_spam(dis, ctx, conf.as_ref(), orig).await;
            r.log_error();
            if let Ok(true) = r {
                // tell em to shut up
                orig.react(ctx, Unicode("⚠️".to_string())).await.map_err(crate::error::Error::from).log_error();
            }
        }

        let finish = start.elapsed();
        trace!("message pressure was {:.3}, took {:?}, {:?} of which was cache", lp.raw(), finish, pre_mess);
        trace!("user pressure is {:?}", pres.as_ref());
        Ok(())
    }
}

async fn mute_for_spam(dis: &Dispatch, ctx: &Context, conf: &SpamConfig, orig: &Message) -> crate::error::Result<bool> {

    // Ignore if this is the guild owner.
    let guild = orig.guild(ctx).await.ok_or(GuildNotInCache)?;
    if guild.owner_id == orig.author.id {
        trace!("not muting guild owner");
        return Ok(false);
    }
    let db = dis.db(guild.id);

    let mod_role = dis.config_value_t::<VerifiedRole>(PRIV_ROLE)?
        .get(&db)
        .await?;

    let mem = orig.member.clone().unwrap();
    if let Some(r) = mod_role {
        let r = *r;
        if mem.roles.contains(&r.into_inner()) {
            trace!("not muting moderator");
            return Ok(false);
        }
    }

    let ignore_role = dis.config_value_t::<VerifiedRole>(SPAM_IGNORE_ROLE)?
        .get(&db)
        .await?;

    if let Some(r) = ignore_role {
        let r = *r;
        if mem.roles.contains(&r.into_inner()) {
            trace!("not muting ignore role");
            return Ok(false);
        }
    }

    let duration = if conf.silence_timeout > Duration::from_secs(0) {
        Some(conf.silence_timeout.into())
    } else {
        None
    };

    let full_mem = orig.member(ctx).await?;
    let me = dis.bot().await;
    let action = ModAction::new(full_mem, orig.channel_id, me, ActionKind::Mute)
        .with_duration(duration)
        .with_reason("Spam")
        .with_original_message(orig.id);
    action.act(dis, ctx).await?;
    action.report_action(dis, ctx).await.map(|_| true)
}