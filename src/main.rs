use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            macros::{command, group},
            CommandResult,
        },
        StandardFramework,
    },
    model::channel::Message,
};
use songbird::{
    input::{Codec, Container, Input},
    SerenityInit,
};
use std::{
    env,
    process::{Command, Stdio},
};

#[group]
#[commands(join)]
struct General;

struct Handler;

#[tokio::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN have to be set");
    let framework = StandardFramework::new()
        .configure(|c| c.prefix("~"))
        .group(&GENERAL_GROUP);

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird()
        .await
        .expect("Failed to create client");
    let _ = client
        .start()
        .await
        .map_err(|err| println!("error: {:?}", err));
}

#[async_trait]
impl EventHandler for Handler {}

#[command]
async fn join(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let channel_id = guild
        .voice_states
        .get(&msg.author.id)
        .and_then(|vs| vs.channel_id);
    let connect_to = match channel_id {
        Some(channel) => channel,
        None => {
            return Ok(());
        }
    };
    let manager = songbird::get(ctx)
        .await
        .expect("Failed to create voice client");
    let _ = manager.join(guild.id, connect_to).await;
    if let Some(handler) = manager.get(guild.id) {
        let mut handler = handler.lock().await;
        let recorder = Command::new("parec")
            .args(&[
                "--format=float32le",
                "--rate=48000",
                "--channels=1",
                "-d",
                "audst.monitor",
                "-r",
            ])
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let source = Input::new(
            false,
            recorder.into(),
            Codec::FloatPcm,
            Container::Raw,
            Default::default(),
        );
        handler.play_source(source);
    } else {
        panic!("");
    }
    Ok(())
}
