use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mongodb::Collection;
use mongodb::bson::doc;
use serde::{Deserialize, Serialize};
use serenity::all::*;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};

type BoxResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DISCORD_CHANNELS_URL: &str = "https://discord.com/channels";

/// One open poll.
#[derive(Serialize, Deserialize)]
struct Poll {
    #[serde(rename = "_id")]
    message: i64,
    channel: i64,
    guild: i64,
    question: String,
    role: String,
    closes_at: i64, // unix seconds
    answers: Vec<Answer>,
    /// user id -> index into `answers`
    votes: HashMap<String, i32>,
    creator: i64,
    role_id: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Answer {
    label: String,
    yes: bool,
}

impl Poll {
    /// Number of votes per answer, in `answers` order.
    fn tally(&self) -> Vec<usize> {
        let mut tally = vec![0; self.answers.len()];
        for &a in self.votes.values() {
            if let Some(n) = tally.get_mut(a as usize) {
                *n += 1;
            }
        }
        tally
    }

    /// The poll message: question, live results, and whether it's still open.
    fn text(&self, role: Option<RoleId>) -> String {
        let tally = self.tally();
        let total: usize = tally.iter().sum();
        let mut s = match role {
            Some(r) => format!(
                "🔒 **{}**\n**This poll is closed.** Green answers got {}.\n\n",
                self.question,
                r.mention()
            ),
            None => format!(
                "📊 **{}**\nGreen answers get the `{}` role. You can change your answer until the poll closes <t:{}:R>.\n\n",
                self.question, self.role, self.closes_at
            ),
        };
        for (a, &n) in self.answers.iter().zip(&tally) {
            let pct = (n * 100).checked_div(total).unwrap_or(0);
            let filled = (pct + 5) / 10;
            s += &format!(
                "{} **{}**\n`{}{}` {n} ({pct}%)\n",
                if a.yes { "🟢" } else { "⚪" },
                a.label,
                "█".repeat(filled),
                "░".repeat(10 - filled)
            );
        }
        s += &format!("\n{total} vote{}", if total == 1 { "" } else { "s" });
        s
    }
}

/// One button per answer, 5 per row; green ones give the role.
fn buttons(answers: &[Answer], disabled: bool) -> Vec<CreateActionRow> {
    answers
        .chunks(5)
        .enumerate()
        .map(|(row, chunk)| {
            let buttons = chunk.iter().enumerate().map(|(i, a)| {
                let style = if a.yes {
                    ButtonStyle::Success
                } else {
                    ButtonStyle::Secondary
                };
                CreateButton::new(format!("answer:{}", row * 5 + i))
                    .label(&a.label)
                    .style(style)
                    .disabled(disabled)
            });
            CreateActionRow::Buttons(buttons.collect())
        })
        .collect()
}

/// Builds the answer list from the two form fields, one answer per line.
fn parse_answers(yes: &str, no: &str) -> std::result::Result<Vec<Answer>, String> {
    let lines = |s: &str, yes: bool| {
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| Answer {
                label: l.into(),
                yes,
            })
            .collect::<Vec<_>>()
    };
    let answers: Vec<_> = lines(yes, true)
        .into_iter()
        .chain(lines(no, false))
        .collect();
    if !answers.iter().any(|a| a.yes) {
        return Err("Add at least one answer that gives the role.".into());
    }
    if answers.len() > 10 {
        return Err("At most 10 answers in total.".into());
    }
    if let Some(a) = answers.iter().find(|a| a.label.chars().count() > 80) {
        return Err(format!("`{}` is too long (80 characters max).", a.label));
    }
    Ok(answers)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Parses the form's close field into a unix timestamp. Accepts a French date
/// (`16/12`, `16/12/2026`: the poll stays open all that day) or a duration (`1d 6h`).
fn parse_close<Tz: TimeZone>(input: &str, now: DateTime<Tz>) -> std::result::Result<i64, String> {
    let closes_at = if input.contains('/') {
        parse_date(input, &now)?
    } else {
        let d = humantime::parse_duration(input).map_err(|e| {
            format!("Can't read `{input}` ({e}). Try `16/12`, `16/12/2026`, `1d 6h` or `90m`.")
        })?;
        now.timestamp()
            .saturating_add(d.as_secs().min(i64::MAX as u64) as i64)
    };
    match closes_at - now.timestamp() {
        ..=0 => Err(format!("`{input}` is already past.")),
        1..60 => Err("The poll must stay open at least 1 minute.".into()),
        s if s > 365 * 86400 => Err("The poll can stay open at most 365 days.".into()),
        _ => Ok(closes_at),
    }
}

/// `dd/mm` or `dd/mm/yyyy` -> the midnight that ends that day, in `now`'s time zone.
/// Without a year, it's the next time that date comes around.
fn parse_date<Tz: TimeZone>(input: &str, now: &DateTime<Tz>) -> std::result::Result<i64, String> {
    let bad =
        || format!("`{input}` isn't a valid date. Use `dd/mm` or `dd/mm/yyyy`, e.g. `16/12`.");
    let parts: Vec<u32> = input
        .split('/')
        .map(|p| p.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| bad())?;
    let end_of_day = |year: i32| {
        let next_day = NaiveDate::from_ymd_opt(year, parts[1], parts[0])?.succ_opt()?;
        let midnight = next_day.and_hms_opt(0, 0, 0)?;
        Some(
            midnight
                .and_local_timezone(now.timezone())
                .earliest()?
                .timestamp(),
        )
    };
    match parts[..] {
        [_, _, year] => end_of_day(year as i32).ok_or_else(bad),
        [_, _] => match end_of_day(now.year()) {
            Some(t) if t > now.timestamp() => Ok(t),
            _ => end_of_day(now.year() + 1).ok_or_else(bad),
        },
        _ => Err(bad()),
    }
}

/// Link that jumps to a message in a server channel.
fn message_link(guild: i64, channel: i64, message: i64) -> String {
    format!("{DISCORD_CHANNELS_URL}/{guild}/{channel}/{message}")
}

/// The value the user typed into a form field.
fn field<'a>(form: &'a ModalInteraction, id: &str) -> &'a str {
    form.data
        .components
        .iter()
        .flat_map(|row| &row.components)
        .find_map(|c| match c {
            ActionRowComponent::InputText(t) if t.custom_id == id => t.value.as_deref(),
            _ => None,
        })
        .unwrap_or_default()
        .trim()
}

/// The form `/poll` opens.
fn poll_form() -> CreateModal {
    let input = |style, label: &str, id: &str| CreateInputText::new(style, label, id);
    CreateModal::new("poll_form", "New poll").components(vec![
        CreateActionRow::InputText(
            input(InputTextStyle::Short, "Question", "question")
                .placeholder("Coming to the trip?")
                .max_length(300),
        ),
        CreateActionRow::InputText(
            input(InputTextStyle::Short, "Role to give", "role")
                .placeholder("trip")
                .max_length(100),
        ),
        CreateActionRow::InputText(
            input(
                InputTextStyle::Short,
                "Closes on (date) or open for (duration)",
                "closes",
            )
            .placeholder("16/12, 16/12/2026, 1d 6h, 90m...")
            .value("1d"),
        ),
        CreateActionRow::InputText(
            input(
                InputTextStyle::Paragraph,
                "Answers that give the role (one per line)",
                "yes",
            )
            .value("Yes for sure\nNot sure yet"),
        ),
        CreateActionRow::InputText(
            input(
                InputTextStyle::Paragraph,
                "Other answers (one per line)",
                "no",
            )
            .value("No")
            .required(false),
        ),
    ])
}

fn message(text: impl Into<String>) -> CreateInteractionResponseMessage {
    CreateInteractionResponseMessage::new().content(text)
}

/// Records (or changes) a user's answer. Returns the updated poll and a reply for the
/// voter, or `None` if the poll is closed.
async fn vote(
    polls: &Collection<Poll>,
    message: i64,
    user: i64,
    answer: i32,
) -> mongodb::error::Result<Option<(Poll, String)>> {
    let before = polls
        .find_one_and_update(
            doc! { "_id": message, "closing": { "$ne": true } },
            doc! { "$set": { format!("votes.{user}"): answer } },
        )
        .await?;
    let Some(mut poll) = before else {
        return Ok(None);
    };
    let label = poll
        .answers
        .get(answer as usize)
        .map(|a| a.label.clone())
        .unwrap_or_default();
    let reply = match poll.votes.insert(user.to_string(), answer) {
        Some(prev) if prev == answer => format!("You already answered **{label}**."),
        Some(_) => format!("Changed your answer to **{label}**."),
        None => format!("You answered **{label}**."),
    };
    Ok(Some((poll, reply)))
}

/// Sleeps until the poll's deadline, then closes it, retrying every 5 minutes on failure.
fn schedule_close(http: Arc<Http>, polls: Collection<Poll>, id: i64, closes_at: i64) {
    tokio::spawn(async move {
        let mut wait = closes_at.saturating_sub(now()).max(0) as u64;
        loop {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match close_poll(&http, &polls, id).await {
                Ok(()) => return,
                Err(e) => eprintln!("closing poll {id} failed, retrying in 5 min: {e}"),
            }
            wait = 300;
        }
    });
}

async fn claim(polls: &Collection<Poll>, id: i64) -> mongodb::error::Result<Option<Poll>> {
    polls
        .find_one_and_update(
            doc! { "_id": id, "closing": { "$ne": true } },
            doc! { "$set": { "closing": true } },
        )
        .await
}

async fn close_poll(http: &Http, polls: &Collection<Poll>, id: i64) -> BoxResult<()> {
    let Some(poll) = claim(polls, id).await? else {
        return Ok(());
    };
    let result = finish_close(http, polls, &poll).await;
    if result.is_err() {
        polls
            .update_one(doc! { "_id": id }, doc! { "$set": { "closing": false } })
            .await?;
    }
    result
}

async fn finish_close(http: &Http, polls: &Collection<Poll>, poll: &Poll) -> BoxResult<()> {
    let id = poll.message;
    let guild = GuildId::new(poll.guild as u64);
    let role = match poll.role_id {
        Some(r) => RoleId::new(r as u64),
        None => {
            let role = guild
                .create_role(http, EditRole::new().name(&poll.role))
                .await?;
            polls
                .update_one(
                    doc! { "_id": id },
                    doc! { "$set": { "role_id": role.id.get() as i64 } },
                )
                .await?;
            role.id
        }
    };
    let mut given = 0;
    for (user, &answer) in &poll.votes {
        if !poll.answers.get(answer as usize).is_some_and(|a| a.yes) {
            continue;
        }
        let Ok(user) = user.parse::<u64>() else {
            continue;
        };
        given += 1;
        if let Err(e) = http
            .add_member_role(guild, UserId::new(user), role, Some("survey-bro poll"))
            .await
        {
            eprintln!("could not give role to {user}: {e}");
        }
    }

    let channel = ChannelId::new(poll.channel as u64);
    let message = MessageId::new(id as u64);
    let edit = EditMessage::new()
        .content(poll.text(Some(role)))
        .components(buttons(&poll.answers, true));
    channel.edit_message(http, message, edit).await?;

    let announce = CreateMessage::new()
        .content(format!(
            "🔒 The poll **{}** has closed! {given} {} got {}.",
            poll.question,
            if given == 1 { "person" } else { "people" },
            role.mention()
        ))
        .reference_message((channel, message))
        .allowed_mentions(CreateAllowedMentions::new());
    channel.send_message(http, announce).await?;

    polls.delete_one(doc! { "_id": id }).await?;
    Ok(())
}

struct Handler {
    polls: Collection<Poll>,
}

impl Handler {
    async fn start_poll(&self, ctx: &Context, form: &ModalInteraction) -> BoxResult<()> {
        let question = field(form, "question").to_owned();
        let role = field(form, "role").to_owned();
        let refuse = |text: String| {
            let msg = message(text).ephemeral(true);
            form.create_response(&ctx.http, CreateInteractionResponse::Message(msg))
        };
        let Some(guild) = form.guild_id else {
            return Ok(refuse("Polls only work in a server.".into()).await?);
        };
        let closes_at = match parse_close(field(form, "closes"), Local::now()) {
            Ok(t) => t,
            Err(e) => return Ok(refuse(e).await?),
        };
        let answers = match parse_answers(field(form, "yes"), field(form, "no")) {
            Ok(a) => a,
            Err(e) => return Ok(refuse(e).await?),
        };
        let mut poll = Poll {
            message: 0, // known once posted
            channel: 0,
            guild: guild.get() as i64,
            question,
            role,
            closes_at,
            answers,
            votes: HashMap::new(),
            creator: form.user.id.get() as i64,
            role_id: None,
        };
        let post = message(poll.text(None)).components(buttons(&poll.answers, false));
        form.create_response(&ctx.http, CreateInteractionResponse::Message(post))
            .await?;
        let msg = form.get_response(&ctx.http).await?;
        poll.message = msg.id.get() as i64;
        poll.channel = msg.channel_id.get() as i64;
        self.polls.insert_one(&poll).await?;
        schedule_close(
            ctx.http.clone(),
            self.polls.clone(),
            poll.message,
            poll.closes_at,
        );

        // DM the creator a close button to preempt the normal closing date.
        let close = CreateButton::new(format!("close:{}", poll.message))
            .label("Close poll now")
            .style(ButtonStyle::Danger);
        let link = message_link(poll.guild, poll.channel, poll.message);
        let dm = CreateMessage::new()
            .content(format!(
                "Your poll **{}** is open: {link}\nClose it early and hand out the `{}` role:",
                poll.question, poll.role
            ))
            .button(close);
        if let Err(e) = form.user.direct_message(&ctx.http, dm).await {
            eprintln!(
                "could not DM {} the close button for poll {}: {e}",
                form.user.id, poll.message
            );
        }
        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let cmd = CreateCommand::new("poll").description(
            "Create a poll; people who pick certain answers get a role when it closes",
        );
        match Command::create_global_command(&ctx.http, cmd).await {
            Ok(_) => println!("{} ready, /poll registered", ready.user.name),
            Err(e) => eprintln!("failed to register /poll: {e}"),
        }

        if let Err(e) = self
            .polls
            .update_many(
                doc! { "closing": true },
                doc! { "$set": { "closing": false } },
            )
            .await
        {
            eprintln!("could not release interrupted closes: {e}");
        }
        let mut cursor = match self.polls.find(doc! {}).await {
            Ok(c) => c,
            Err(e) => return eprintln!("could not load open polls: {e}"),
        };
        while let Ok(true) = cursor.advance().await {
            match cursor.deserialize_current() {
                Ok(p) => {
                    schedule_close(ctx.http.clone(), self.polls.clone(), p.message, p.closes_at)
                }
                Err(e) => eprintln!("skipping unreadable poll: {e}"),
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) if cmd.data.name == "poll" => {
                let response = if cmd.guild_id.is_some() {
                    CreateInteractionResponse::Modal(poll_form())
                } else {
                    CreateInteractionResponse::Message(
                        message("Polls only work in a server.").ephemeral(true),
                    )
                };
                if let Err(e) = cmd.create_response(&ctx.http, response).await {
                    eprintln!("could not open poll form: {e}");
                }
            }
            Interaction::Modal(form) if form.data.custom_id == "poll_form" => {
                if let Err(e) = self.start_poll(&ctx, &form).await {
                    eprintln!("poll failed: {e}");
                }
            }
            Interaction::Component(c) if c.data.custom_id.starts_with("answer:") => {
                let Ok(answer) = c.data.custom_id["answer:".len()..].parse() else {
                    return;
                };
                if let Err(e) = self.on_vote(&ctx, &c, answer).await {
                    eprintln!("vote failed: {e}");
                }
            }
            Interaction::Component(c) if c.data.custom_id.starts_with("close:") => {
                let Ok(id) = c.data.custom_id["close:".len()..].parse::<i64>() else {
                    return;
                };
                if let Err(e) = self.close_early(&ctx, &c, id).await {
                    eprintln!("closing poll {id} early failed: {e}");
                }
            }
            _ => {}
        }
    }
}

impl Handler {
    async fn on_vote(&self, ctx: &Context, c: &ComponentInteraction, answer: i32) -> BoxResult<()> {
        let result = vote(
            &self.polls,
            c.message.id.get() as i64,
            c.user.id.get() as i64,
            answer,
        )
        .await;
        let private =
            |text: &str| CreateInteractionResponse::Message(message(text).ephemeral(true));
        match result {
            Ok(Some((poll, reply))) => {
                let update = CreateInteractionResponseMessage::new().content(poll.text(None));
                c.create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(update))
                    .await?;
                let followup = CreateInteractionResponseFollowup::new()
                    .content(reply)
                    .ephemeral(true);
                c.create_followup(&ctx.http, followup).await?;
            }
            Ok(None) => {
                c.create_response(&ctx.http, private("This poll is closed."))
                    .await?
            }
            Err(e) => {
                c.create_response(&ctx.http, private("Something went wrong, try again."))
                    .await?;
                return Err(e.into());
            }
        }
        Ok(())
    }

    async fn close_early(&self, ctx: &Context, c: &ComponentInteraction, id: i64) -> BoxResult<()> {
        let update = |text: &str| {
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(text)
                    .components(vec![]),
            )
        };
        let poll = self.polls.find_one(doc! { "_id": id }).await?;
        let text = match poll {
            None => "This poll is already closed.",
            Some(p) if p.creator != c.user.id.get() as i64 => {
                "Only the poll's creator can close it."
            }
            Some(_) => {
                c.create_response(&ctx.http, update("Closing the poll..."))
                    .await?;
                if let Err(e) = close_poll(&ctx.http, &self.polls, id).await {
                    let retry = CreateButton::new(format!("close:{id}"))
                        .label("Close poll now")
                        .style(ButtonStyle::Danger);
                    let edit = EditInteractionResponse::new()
                        .content("Couldn't close the poll, try again in a moment.")
                        .components(vec![CreateActionRow::Buttons(vec![retry])]);
                    c.edit_response(&ctx.http, edit).await?;
                    return Err(e);
                }
                c.edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content("Poll closed."),
                )
                .await?;
                return Ok(());
            }
        };
        c.create_response(&ctx.http, update(text)).await?;
        Ok(())
    }
}

async fn polls_collection() -> mongodb::error::Result<Collection<Poll>> {
    let uri = std::env::var("MONGODB_URI").expect("set MONGODB_URI");
    Ok(mongodb::Client::with_uri_str(uri)
        .await?
        .database("survey_bro")
        .collection("polls"))
}

#[tokio::main]
async fn main() {
    let token = std::env::var("DISCORD_TOKEN").expect("set DISCORD_TOKEN");
    let polls = polls_collection().await.expect("bad MONGODB_URI");
    let mut client = Client::builder(token, GatewayIntents::empty())
        .event_handler(Handler { polls })
        .await
        .expect("failed to build client");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    println!("connecting to Discord...");
    tokio::select! {
        result = client.start() => result.expect("client crashed"),
        _ = sigterm.recv() => println!("got SIGTERM, shutting down"),
        _ = tokio::signal::ctrl_c() => println!("got Ctrl+C, shutting down"),
    }
}

#[cfg(test)]
#[test]
fn parse_close_examples() {
    let paris = chrono::FixedOffset::east_opt(3600).unwrap();
    let at = |y, m, d, h| paris.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
    let now = at(2026, 11, 26, 12);
    let ok = |s: &str| parse_close(s, now).unwrap();

    assert_eq!(ok("16/12"), at(2026, 12, 17, 0).timestamp());
    assert_eq!(ok("16/12/2026"), at(2026, 12, 17, 0).timestamp());
    assert_eq!(ok(" 1/1 "), at(2027, 1, 2, 0).timestamp());
    assert_eq!(ok("26/11"), at(2026, 11, 27, 0).timestamp());
    assert_eq!(ok("25/11"), at(2027, 11, 26, 0).timestamp());
    assert_eq!(ok("1d 6h"), now.timestamp() + 30 * 3600);

    let err = |s: &str| parse_close(s, now).unwrap_err();
    assert!(err("25/11/2026").contains("already past"));
    assert!(err("31/02").contains("isn't a valid date"));
    assert!(err("16/12/2028").contains("at most 365 days"));
    assert!(err("ab/12").contains("isn't a valid date"));
    assert!(err("16/12/2026/1").contains("isn't a valid date"));
    assert!(err("30s").contains("at least 1 minute"));
    assert!(err("tomorrow").contains("Can't read"));
}
