use crate::module::{Module, ModInfo, Sensitivity, UnimplementedModule};
use serenity::client::Context;
use serenity::model::channel::Message;
use crate::dispatch::Dispatch;
use crate::dispatch::config::{Value, VerifiedChannel, VerifiedRole};
use once_cell::sync::Lazy;
use structopt::StructOpt;
use crate::util::constraints::{ConstrainedU64, AtMostU64};
use std::time::Duration;
use serenity::model::id::{UserId, GuildId, MessageId, ChannelId};
use chrono::Utc;
use serenity::model::guild::Member;
use crate::db::{DbContext};
use std::borrow::{Cow, Borrow};
use serenity::utils::{MessageBuilder, Color};
use serenity::builder::{CreateEmbed, CreateMessage};
use serenity::model::guild::Target::User;
use serenity::model::misc::Mentionable;
use std::fmt;
use std::fmt::Formatter;
use crate::error::Error;
use crate::util::ClapExt;

pub struct ModerationModule;

pub const TIMED_ACTION_KEY: &'static str = "timed";

#[derive(Debug, StructOpt)]
enum Action {
    /// Warn a user and make a note in the mod log about it.
    Warn(CommonOpts),
    /// Kick a user from the server.
    Kick(CommonOpts),
    /// Ban a user from the server.
    Ban {
        #[structopt(flatten)]
        common: CommonOpts,
        /// How long the user should be banned for. Specified in human format, i.e. "5d 2h 5m"
        /// Max 100 years, min 1 minute.
        #[structopt(short = "d")]
        duration: Option<humantime::Duration>,
        #[structopt(short = "m")]
        /// How many days of messages from the user should be deleted.
        delete_messages: Option<AtMostU64<7>>
    },
    /// Bans a user with max number of days for message deletion, then unbans them.
    /// Useful for deleting spam.
    SoftBan(CommonOpts),
    /// Adds the muted user role to a user.
    Mute {
        #[structopt(flatten)]
        common: CommonOpts,
        #[structopt(short = "d")]
        /// How long the user should be muted for. Specified in human format, i.e. "5d 2h 5m"
        /// Max 100 years, min 1 minute.
        duration: Option<humantime::Duration>
    }
}

#[derive(Debug, StructOpt)]
struct CommonOpts {
    /// Which user the action should apply to.
    user: String,
    /// Why the action is being taken.
    reason: String,
}

#[derive(Debug, StructOpt)]
/// Command for moderating users.
pub struct ModOpt {
    #[structopt(subcommand)]
    action: Action,
}

const MOD_CHANNEL: &'static str = "mod_log_channel";

const MUTE_ROLE: &'static str = "mute_role";

#[async_trait::async_trait]
impl Module for ModerationModule {
    fn info(&self) -> &ModInfo {
        static INFO: Lazy<ModInfo> = Lazy::new(|| ModInfo::with_name("mod")
            .with_sensitivity(Sensitivity::High)
            .with_command(true)
            .with_config_value(Value::<VerifiedChannel>::new(MOD_CHANNEL, "Channel for logging moderation actions."))
            .with_config_value(Value::<VerifiedRole>::new(MUTE_ROLE, "Role to assign to muted users."))
        );

        &INFO
    }

    async fn process(&self, dis: &Dispatch, ctx: &Context, orig: &Message, command: Vec<String>) -> crate::error::Result<()> {
        let opts = ModOpt::from_iter_with_help(command)?;
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ActionKind {
    Warn,
    Kick,
    SoftBan,
    Ban,
    Mute
}

impl ActionKind {
    pub const SAFETY_YELLOW: Color = Color::new(0xEED202);
    pub const SAFETY_ORANGE: Color = Color::new(0xFF6700);
    pub const TRAFFIC_RED: Color = Color::new(0xB8D113);

    pub const fn color(&self) -> Color {
        match self {
            ActionKind::Warn => Self::SAFETY_YELLOW,
            ActionKind::Kick => Self::SAFETY_ORANGE,
            ActionKind::SoftBan => Color::FABLED_PINK,
            ActionKind::Ban => Self::TRAFFIC_RED,
            ActionKind::Mute => Color::DARK_BLUE,
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            ActionKind::Warn => {"warning"}
            ActionKind::Kick => {"kick"}
            ActionKind::SoftBan => {"soft ban"}
            ActionKind::Ban => {"ban"}
            ActionKind::Mute => {"mute"}
        }
    }

    pub const fn title_name(&self) -> &'static str {
        match self {
            ActionKind::Warn => {"Warning"}
            ActionKind::Kick => {"Kick"}
            ActionKind::SoftBan => {"Soft ban"}
            ActionKind::Ban => {"Ban"}
            ActionKind::Mute => {"Mute"}
        }
    }

    pub const fn has_duration(&self) -> bool {
        match self {
            ActionKind::Ban |
            ActionKind::Mute => {true}
            _ => false
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModAction {
    user: Member,
    action: ActionKind,
    moderator: UserId,
    channel: ChannelId,
    reason: Option<Cow<'static, str>>,
    original_message: Option<MessageId>,
    duration: Option<Duration>,
}

impl ModAction {
    pub fn user(&self) -> &Member {
        &self.user
    }
    pub fn action(&self) -> ActionKind {
        self.action
    }
    pub fn moderator(&self) -> UserId {
        self.moderator
    }
    pub fn reason(&self) -> &Option<Cow<'static, str>> {
        &self.reason
    }
    pub fn original_message(&self) -> Option<MessageId> {
        self.original_message
    }
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }
    pub fn guild(&self) -> GuildId { self.user().guild_id }
}

impl ModAction {
    pub fn new(mem: impl Borrow<Member>, channel_id: ChannelId, moderator: UserId, action: ActionKind) -> Self {
        ModAction {
            user: mem.borrow().clone(),
            action,
            moderator,
            channel: channel_id,
            reason: None,
            original_message: None,
            duration: None
        }
    }

    pub fn with_duration(mut self, duration: Option<Duration>) -> Self {
        self.duration = duration;
        self
    }

    pub fn with_reason(mut self, reason: impl Into<Cow<'static, str>>) -> Self {
        self.reason = reason.into().into();
        self
    }

    pub fn with_original_message(mut self, message: MessageId) -> Self {
        self.original_message = Some(message);
        self
    }

    pub fn create_embed(&self, embed: &mut CreateEmbed) {
        let user = format!("{} ({})", self.user.display_name(), self.user.user.id);
        let moderator = self.moderator.mention();
        let reason = self.reason.clone().unwrap_or("No reason specified.".into());

        embed.color(self.action.color())
            .title(self.action.title_name())
            .field("User", user, false)
            .field("Reason", reason, false)
            .field("Moderator", moderator, false)
            .field("Channel", self.channel.mention(), false);

        if self.action.has_duration() {
            let dur = self.duration.as_ref()
                .map(|d| humantime::format_duration(d.clone())
                .to_string()
                    .into())
                .unwrap_or(Cow::from("indefinite"));

            embed.field("Duration", dur, false);
        }

        if let Some(m) = self.original_message {
            let url = format!("https://discord.com/channels/{gid}/{chan}/{mess}",
                gid=self.user.guild_id,
                chan=self.channel,
                mess=m
            );
            embed.field("In response to", url, false);
        }
    }
}

impl_err!(NoModChannelSet, "No mod channel has been set for this guild (mod_log_channel).", true);
impl_err!(NoMuteRoleSet, "No mute role has been set for this guild (mute_role).", true);

pub async fn mute_user(dis: &Dispatch, ctx: &Context, action: &ModAction) -> crate::error::Result<()> {
    let cfg_db = DbContext::new(dis.pool(), action.guild());
    let mute_role = dis.config_value_t::<VerifiedRole>(MUTE_ROLE)?
        .get(&cfg_db)
        .await?
        .ok_or(NoMuteRoleSet)?;
    let mut mem = action.user().clone();
    mem.add_role(ctx, mute_role.into_inner()).await?;
    todo!("add store for non-indefinite case");
    report_action(dis, ctx, action).await
}

pub async fn report_action(dis: &Dispatch, ctx: &Context, action: &ModAction) -> crate::error::Result<()> {
    let mod_channel_v = dis.config_value_t::<VerifiedChannel>(MOD_CHANNEL)?;
    let cfg_db = DbContext::new(dis.pool(), action.guild());
    let mod_channel = mod_channel_v.get(&cfg_db)
        .await?
        .ok_or(NoModChannelSet)?;
    mod_channel.into_inner().send_message(ctx, |e| {
        e.embed(|emb| { action.create_embed(emb); emb })
    }).await?;
    Ok(())
}